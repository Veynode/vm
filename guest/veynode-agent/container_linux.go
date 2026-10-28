//go:build linux

// OCI micro-VM init. Runs as PID 1 from the VeynodeVM initramfs, assembles the
// container root from the read-only image and the writable disk, starts the
// guest service for exec, runs the image entrypoint and powers off with its
// exit status. Host-visible events are console lines prefixed "@@veynodevm ".
package main

import (
	"bufio"
	"bytes"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
	"unsafe"
)

const containerConfigMagic = "VEYNODEVM-CONFIG-V1\n"

// RNDADDENTROPY: _IOW('R', 0x03, int[2]).
const rndAddEntropy = 0x40085203

const containerRootDir = "/newroot"
const containerRunDir = "/run/veynodevm"

// Fixed device order attached by the host: writable disk, image, config, modules.
const (
	writableDevice = "/dev/vda"
	imageDevice    = "/dev/vdb"
	configDevice   = "/dev/vdc"
	modulesDevice  = "/dev/vdd"
)

type containerVolume struct {
	Tag      string `json:"tag"`
	Target   string `json:"target"`
	ReadOnly bool   `json:"read_only"`
	FS       string `json:"fs"`
}

type containerConfig struct {
	Version          int               `json:"version"`
	ID               string            `json:"id"`
	Hostname         string            `json:"hostname"`
	Argv             []string          `json:"argv"`
	Env              []string          `json:"env"`
	Workdir          string            `json:"workdir"`
	User             string            `json:"user"`
	Volumes          []containerVolume `json:"volumes"`
	AnonymousVolumes []string          `json:"anonymous_volumes"`
	Transport        string            `json:"transport"`
	GPU              bool              `json:"gpu"`
	Engine           bool              `json:"engine"`
	Desktop          bool              `json:"desktop,omitempty"`
	DNS              []string          `json:"dns,omitempty"`
	MTU              int               `json:"mtu,omitempty"`
	// Interactive sessions (session_linux.go): the token the host presents,
	// and whether the entrypoint gets a pty (-t) or an open stdin (-i).
	SessionToken string `json:"session_token,omitempty"`
	TTY          bool   `json:"tty,omitempty"`
	Stdin        bool   `json:"stdin,omitempty"`
	// Init "systemd" ("full system" mode) prepares the root as usual, then
	// makes argv (the image's /sbin/init) PID 1 inside it; see systemd_linux.go.
	Init string `json:"init,omitempty"`
}

type containerEnvFile struct {
	Env     []string `json:"env"`
	Workdir string   `json:"workdir"`
}

func marker(kind, value string) {
	fmt.Printf("@@veynodevm %s %s\n", kind, value)
}

// creditSeed adds the host's seed to the kernel's entropy pool and credits it,
// so the CRNG is ready at once instead of after seconds of jitter collection
// (a micro-VM has no other entropy source this early; getrandom blocks until then).
func creditSeed(raw []byte) {
	seed := seedFrom(raw)
	if seed == nil {
		return
	}
	f, err := os.OpenFile("/dev/urandom", os.O_WRONLY, 0)
	if err != nil {
		marker("warning", "entropy seed: "+err.Error())
		return
	}
	defer f.Close()
	// struct rand_pool_info { int entropy_count; int buf_size; __u32 buf[]; }
	request := make([]byte, 8+len(seed))
	binary.LittleEndian.PutUint32(request[0:], uint32(len(seed)*8))
	binary.LittleEndian.PutUint32(request[4:], uint32(len(seed)))
	copy(request[8:], seed)
	if _, _, errno := syscall.Syscall(syscall.SYS_IOCTL, f.Fd(), rndAddEntropy, uintptr(unsafe.Pointer(&request[0]))); errno != 0 {
		marker("warning", "entropy seed: "+errno.Error())
	}
}

func parseContainerConfig(raw []byte) (containerConfig, error) {
	var config containerConfig
	if !bytes.HasPrefix(raw, []byte(containerConfigMagic)) {
		return config, errors.New("config disk lacks VeynodeVM magic")
	}
	body := bytes.TrimRight(raw[len(containerConfigMagic):], "\x00\n")
	decoder := json.NewDecoder(bytes.NewReader(body))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&config); err != nil {
		return config, fmt.Errorf("invalid container config: %w", err)
	}
	if config.Version != 1 {
		return config, errors.New("unsupported container config version")
	}
	if len(config.Argv) == 0 {
		return config, errors.New("container has no entrypoint or command")
	}
	if config.Transport != "vsock" && config.Transport != "virtio-serial-exec" {
		return config, errors.New("unsupported guest transport")
	}
	if config.Init != "" && config.Init != "systemd" {
		return config, fmt.Errorf("unsupported init %q", config.Init)
	}
	if config.Init == "systemd" && !filepath.IsAbs(config.Argv[0]) {
		return config, errors.New("init path must be absolute")
	}
	for _, path := range append([]string{config.Workdir}, config.AnonymousVolumes...) {
		if path != "" && !filepath.IsAbs(path) {
			return config, fmt.Errorf("container paths must be absolute: %q", path)
		}
	}
	if config.MTU != 0 && (config.MTU < 576 || config.MTU > 9000) {
		return config, errors.New("mtu must be 576..9000")
	}
	if len(config.DNS) > 3 {
		return config, errors.New("at most 3 DNS servers")
	}
	for _, server := range config.DNS {
		if net.ParseIP(server) == nil {
			return config, fmt.Errorf("invalid DNS server %q", server)
		}
	}
	for _, v := range config.Volumes {
		if !filepath.IsAbs(v.Target) || v.Tag == "" || (v.FS != "virtiofs" && v.FS != "9p") {
			return config, fmt.Errorf("invalid volume %q", v.Target)
		}
	}
	return config, nil
}

// insideRoot maps an absolute container path to the host-root path, refusing
// any path that would leave the container root lexically.
func insideRoot(root, path string) (string, error) {
	clean := filepath.Clean("/" + path)
	joined := filepath.Join(root, clean)
	if joined != root && !strings.HasPrefix(joined, root+"/") {
		return "", fmt.Errorf("path escapes container root: %q", path)
	}
	return joined, nil
}

func mountAt(source, target, fstype string, flags uintptr, data string) error {
	if err := os.MkdirAll(target, 0o755); err != nil {
		return err
	}
	if err := syscall.Mount(source, target, fstype, flags, data); err != nil {
		return fmt.Errorf("mount %s on %s: %w", fstype, target, err)
	}
	return nil
}

func modprobe(names ...string) {
	for _, name := range names {
		if out, err := exec.Command("/sbin/modprobe", "-q", name).CombinedOutput(); err != nil {
			marker("warning", fmt.Sprintf("modprobe %s: %v %s", name, err, strings.TrimSpace(string(out))))
		}
	}
}

func waitForDevice(path string) error {
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(path); err == nil {
			return nil
		}
		time.Sleep(20 * time.Millisecond)
	}
	return fmt.Errorf("device %s did not appear", path)
}

// growWritableLayer grows the mounted file system on `device` to the whole
// device once the host made the disk larger (`veynodevm update --disk-size`).
// A failure is reported and the layer is used at its old size.
func growWritableLayer(device, mountpoint string) {
	f, err := os.Open(device)
	if err != nil {
		return
	}
	defer f.Close()
	super := make([]byte, 1024)
	if _, err := f.ReadAt(super, 1024); err != nil {
		return
	}
	used := ext4Bytes(super)
	size, err := f.Seek(0, io.SeekEnd)
	// Less than a 4 KiB block over: nothing to grow into.
	if err != nil || used == 0 || size-used < 4096 {
		return
	}
	marker("status", "growing writable layer")
	if out, err := exec.Command("/usr/sbin/resize2fs", device).CombinedOutput(); err != nil {
		marker("error", fmt.Sprintf("resize2fs %s (%s): %v %s", device, mountpoint, err, strings.TrimSpace(string(out))))
	}
}

func hasExt4(device string) bool {
	f, err := os.Open(device)
	if err != nil {
		return false
	}
	defer f.Close()
	magic := make([]byte, 2)
	if _, err := f.ReadAt(magic, 1080); err != nil {
		return false
	}
	return magic[0] == 0x53 && magic[1] == 0xef
}

// lookupUser resolves "", "uid", "uid:gid", "name" or "name:group" against
// the container's /etc/passwd and /etc/group.
func lookupUser(root, spec string) (uid, gid uint32, home string, err error) {
	home = "/root"
	if spec == "" {
		return 0, 0, home, nil
	}
	userPart, groupPart, hasGroup := strings.Cut(spec, ":")
	found := false
	if passwd, readErr := os.ReadFile(filepath.Join(root, "etc/passwd")); readErr == nil {
		for _, line := range strings.Split(string(passwd), "\n") {
			fields := strings.Split(line, ":")
			if len(fields) < 7 || (fields[0] != userPart && fields[2] != userPart) {
				continue
			}
			u, errU := strconv.ParseUint(fields[2], 10, 32)
			g, errG := strconv.ParseUint(fields[3], 10, 32)
			if errU == nil && errG == nil {
				uid, gid, home, found = uint32(u), uint32(g), fields[5], true
				break
			}
		}
	}
	if !found {
		u, parseErr := strconv.ParseUint(userPart, 10, 32)
		if parseErr != nil {
			return 0, 0, "", fmt.Errorf("unknown container user %q", userPart)
		}
		uid, gid, home = uint32(u), uint32(u), "/"
	}
	if hasGroup {
		if g, parseErr := strconv.ParseUint(groupPart, 10, 32); parseErr == nil {
			gid = uint32(g)
		} else {
			groups, _ := os.ReadFile(filepath.Join(root, "etc/group"))
			resolved := false
			for _, line := range strings.Split(string(groups), "\n") {
				fields := strings.Split(line, ":")
				if len(fields) >= 3 && fields[0] == groupPart {
					if g, err := strconv.ParseUint(fields[2], 10, 32); err == nil {
						gid, resolved = uint32(g), true
					}
				}
			}
			if !resolved {
				return 0, 0, "", fmt.Errorf("unknown container group %q", groupPart)
			}
		}
	}
	return uid, gid, home, nil
}

// userGroups lists the supplementary groups /etc/group gives the user uid
// (by its /etc/passwd name), as `docker exec -u` adds them; gid is the
// primary group, kept first.
func userGroups(root string, uid, gid uint32) []uint32 {
	name := ""
	if passwd, err := os.ReadFile(filepath.Join(root, "etc/passwd")); err == nil {
		for _, line := range strings.Split(string(passwd), "\n") {
			fields := strings.Split(line, ":")
			if len(fields) >= 7 && fields[2] == strconv.FormatUint(uint64(uid), 10) {
				name = fields[0]
				break
			}
		}
	}
	groups := []uint32{gid}
	if name == "" {
		return groups
	}
	file, _ := os.ReadFile(filepath.Join(root, "etc/group"))
	for _, line := range strings.Split(string(file), "\n") {
		fields := strings.Split(line, ":")
		if len(fields) < 4 {
			continue
		}
		g, err := strconv.ParseUint(fields[2], 10, 32)
		if err != nil || uint32(g) == gid {
			continue
		}
		for _, member := range strings.Split(fields[3], ",") {
			if member == name {
				groups = append(groups, uint32(g))
				break
			}
		}
	}
	return groups
}

// withRequest applies a command's user, environment and working directory,
// like `docker exec -u -e -w`. Only container roots take them; a fixed
// --exec-user stays fixed.
func (identity executionIdentity) withRequest(req request) (executionIdentity, error) {
	if req.User == "" && len(req.Env) == 0 && req.Workdir == "" {
		return identity, nil
	}
	if identity.Mode != "container-root" {
		return identity, errors.New("user, env and workdir apply only to container commands")
	}
	identity.env = mergeEnv(identity.env, req.Env)
	if req.Workdir != "" {
		if !filepath.IsAbs(req.Workdir) {
			return identity, errors.New("workdir must be absolute")
		}
		identity.CWD = req.Workdir
	}
	if req.User != "" {
		uid, gid, home, err := lookupUser(identity.Chroot, req.User)
		if err != nil {
			return identity, err
		}
		identity.User, identity.UID, identity.GID, identity.Home = req.User, uid, gid, home
		identity.credential = &syscall.Credential{Uid: uid, Gid: gid, Groups: userGroups(identity.Chroot, uid, gid)}
		identity.env = withDefaultEnv(identity.env, home)
	}
	return identity, nil
}

// mergeEnv sets each KEY=VALUE of overrides, replacing an earlier KEY.
func mergeEnv(env, overrides []string) []string {
	env = append([]string(nil), env...)
	for _, entry := range overrides {
		key, _, _ := cutEnv(entry)
		filtered := env[:0]
		for _, existing := range env {
			if k, _, _ := cutEnv(existing); k != key {
				filtered = append(filtered, existing)
			}
		}
		env = append(filtered, entry)
	}
	return env
}

func withDefaultEnv(env []string, home string) []string {
	has := func(key string) bool {
		for _, entry := range env {
			if strings.HasPrefix(entry, key+"=") {
				return true
			}
		}
		return false
	}
	out := append([]string(nil), env...)
	if !has("PATH") {
		out = append(out, "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
	}
	if !has("HOME") {
		out = append(out, "HOME="+home)
	}
	return out
}

func loadContainerIdentity(root, envFile string) (executionIdentity, error) {
	if !filepath.IsAbs(root) {
		return executionIdentity{}, errors.New("--container-root must be absolute")
	}
	var file containerEnvFile
	if envFile != "" {
		raw, err := os.ReadFile(envFile)
		if err != nil {
			return executionIdentity{}, err
		}
		if err = json.Unmarshal(raw, &file); err != nil {
			return executionIdentity{}, err
		}
	}
	return containerIdentity(root, withDefaultEnv(file.Env, "/root"), file.Workdir), nil
}

const udhcpcScript = `#!/bin/sh
case "$1" in
  bound|renew)
    ifconfig "$interface" "$ip" netmask "${subnet:-255.255.255.0}" up
    while route del default 2>/dev/null; do :; done
    for r in $router; do route add default gw "$r" dev "$interface"; break; done
    : > /run/veynodevm/resolv.conf
    for d in $dns; do echo "nameserver $d" >> /run/veynodevm/resolv.conf; done
    echo "$ip" > /run/veynodevm/ip
    ;;
  deconfig) ifconfig "$interface" 0.0.0.0 ;;
esac
`

func configureNetwork(config containerConfig) string {
	_ = exec.Command("/bin/busybox", "ifconfig", "lo", "127.0.0.1", "up").Run()
	// Packetization-layer PMTU discovery: recover from paths that drop large
	// segments without ICMP (observed behind some host networks on Apple NAT).
	_ = os.WriteFile("/proc/sys/net/ipv4/tcp_mtu_probing", []byte("1"), 0o644)
	if err := os.WriteFile(containerRunDir+"/udhcpc.script", []byte(udhcpcScript), 0o755); err != nil {
		marker("warning", "network script: "+err.Error())
		return ""
	}
	if config.MTU > 0 {
		if out, err := exec.Command("/bin/busybox", "ifconfig", "eth0", "mtu", strconv.Itoa(config.MTU)).CombinedOutput(); err != nil {
			marker("warning", "mtu: "+strings.TrimSpace(string(out)))
		}
	}
	if out, err := exec.Command("/bin/busybox", "udhcpc", "-i", "eth0", "-n", "-q", "-t", "10", "-T", "1", "-s", containerRunDir+"/udhcpc.script").CombinedOutput(); err != nil {
		marker("warning", "dhcp: "+strings.TrimSpace(string(out)))
		return ""
	}
	ip, _ := os.ReadFile(containerRunDir + "/ip")
	return strings.TrimSpace(string(ip))
}

// bindFile replaces a container file (resolv.conf, hosts, hostname) like Docker.
func bindFile(source, target string) {
	if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
		return
	}
	if info, err := os.Lstat(target); err != nil || info.Mode()&os.ModeSymlink != 0 {
		_ = os.Remove(target)
		if f, err := os.OpenFile(target, os.O_CREATE|os.O_WRONLY, 0o644); err == nil {
			f.Close()
		}
	}
	if err := syscall.Mount(source, target, "", syscall.MS_BIND, ""); err != nil {
		marker("warning", fmt.Sprintf("bind %s: %v", target, err))
	}
}

const cdiSpec = `cdiVersion: "0.6.0"
kind: veynodevm.dev/gpu
devices:
  - name: all
    containerEdits:
      deviceNodes:
        - path: /dev/dri/renderD128
        - path: /dev/dri/card0
`

func prepareRoot(config containerConfig) error {
	modprobe("squashfs", "virtio_net", "af_packet", "overlay")
	if err := waitForDevice(modulesDevice); err != nil {
		return err
	}
	if err := mountAt(modulesDevice, "/.modloop", "squashfs", syscall.MS_RDONLY, ""); err != nil {
		return err
	}
	if err := mountAt("/.modloop/modules", "/lib/modules", "", syscall.MS_BIND|syscall.MS_RDONLY, ""); err != nil {
		return err
	}
	modprobe("ext4", "vsock", "vmw_vsock_virtio_transport", "virtiofs", "9pnet_virtio", "9p", "fuse", "tun")
	if config.GPU || config.Desktop {
		modprobe("virtio-gpu")
	}
	if config.Desktop {
		// Apple Virtualization attaches USB keyboard and pointer; QEMU uses virtio input.
		modprobe("xhci-pci", "usbhid", "hid-generic", "evdev", "virtio-input", "uinput")
		// QEMU's sound card (Config::audio); Apple Virtualization has none.
		modprobe("virtio_snd")
	}
	if config.Engine {
		modprobe("bridge", "br_netfilter", "veth", "nf_tables", "nft_chain_nat", "nft_compat", "ip_tables", "iptable_nat", "iptable_filter", "xt_MASQUERADE", "xt_conntrack", "xt_addrtype", "nf_nat", "nf_conntrack", "ipt_REJECT", "xt_nat")
		_ = os.WriteFile("/proc/sys/net/ipv4/ip_forward", []byte("1"), 0o644)
	}
	if err := mountAt("cgroup2", "/sys/fs/cgroup", "cgroup2", syscall.MS_NOSUID|syscall.MS_NODEV|syscall.MS_NOEXEC, ""); err != nil && !errors.Is(err, syscall.EBUSY) {
		return err
	}
	if !hasExt4(writableDevice) {
		marker("status", "formatting writable layer")
		if out, err := exec.Command("/sbin/mke2fs", "-q", "-F", "-t", "ext4", "-L", "veynodevm", "-E", "lazy_itable_init=1,lazy_journal_init=1,nodiscard", writableDevice).CombinedOutput(); err != nil {
			return fmt.Errorf("mke2fs: %v %s", err, strings.TrimSpace(string(out)))
		}
	}
	if err := mountAt(writableDevice, "/run/disk", "ext4", 0, ""); err != nil {
		return err
	}
	growWritableLayer(writableDevice, "/run/disk")
	for _, dir := range []string{"/run/disk/upper", "/run/disk/work", "/run/disk/volumes"} {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			return err
		}
	}
	if forked, err := adoptWritableLayer("/run/disk", "/run/disk/upper", config.ID); err != nil {
		return err
	} else if forked {
		marker("status", "forked writable layer: new machine identity")
	}
	if err := mountAt(imageDevice, "/run/lower", "squashfs", syscall.MS_RDONLY, ""); err != nil {
		return err
	}
	if err := mountAt("overlay", containerRootDir, "overlay", 0, "lowerdir=/run/lower,upperdir=/run/disk/upper,workdir=/run/disk/work"); err != nil {
		return err
	}
	root := containerRootDir
	for _, m := range []struct {
		src, dst, fs string
		flags        uintptr
		data         string
	}{
		{"proc", "proc", "proc", syscall.MS_NOSUID | syscall.MS_NODEV | syscall.MS_NOEXEC, ""},
		{"sysfs", "sys", "sysfs", syscall.MS_NOSUID | syscall.MS_NODEV | syscall.MS_NOEXEC, ""},
		{"devtmpfs", "dev", "devtmpfs", syscall.MS_NOSUID, ""},
		{"devpts", "dev/pts", "devpts", syscall.MS_NOSUID | syscall.MS_NOEXEC, "newinstance,ptmxmode=0666,mode=0620"},
		{"tmpfs", "dev/shm", "tmpfs", syscall.MS_NOSUID | syscall.MS_NODEV, "mode=1777"},
		{"tmpfs", "run", "tmpfs", syscall.MS_NOSUID | syscall.MS_NODEV, "mode=755"},
		{"cgroup2", "sys/fs/cgroup", "cgroup2", syscall.MS_NOSUID | syscall.MS_NODEV | syscall.MS_NOEXEC, ""},
	} {
		if err := mountAt(m.src, filepath.Join(root, m.dst), m.fs, m.flags, m.data); err != nil {
			return err
		}
	}
	_ = os.Symlink("pts/ptmx", filepath.Join(root, "dev/ptmx.veynodevm"))
	if _, err := os.Stat(filepath.Join(root, "dev/ptmx")); err != nil {
		_ = os.Symlink("pts/ptmx", filepath.Join(root, "dev/ptmx"))
	}
	if err := mountAt("/lib/modules", filepath.Join(root, "lib/modules"), "", syscall.MS_BIND|syscall.MS_RDONLY, ""); err != nil {
		marker("warning", err.Error())
	}
	for index, path := range config.AnonymousVolumes {
		target, err := insideRoot(root, path)
		if err != nil {
			return err
		}
		source := fmt.Sprintf("/run/disk/volumes/%d", index)
		if err = os.MkdirAll(source, 0o755); err != nil {
			return err
		}
		if err = mountAt(source, target, "", syscall.MS_BIND, ""); err != nil {
			return err
		}
	}
	for _, v := range config.Volumes {
		target, err := insideRoot(root, v.Target)
		if err != nil {
			return err
		}
		var flags uintptr
		if v.ReadOnly {
			flags = syscall.MS_RDONLY
		}
		data := ""
		if v.FS == "9p" {
			data = "trans=virtio,version=9p2000.L,msize=262144,cache=mmap"
		}
		if err = mountAt(v.Tag, target, v.FS, flags, data); err != nil {
			return err
		}
	}
	return nil
}

func containerInit() int {
	if os.Getpid() != 1 {
		fmt.Fprintln(os.Stderr, "container-init must run as PID 1")
		return 1
	}
	fail := func(err error) int {
		marker("error", err.Error())
		powerOff(125)
		return 125
	}
	for _, m := range []struct{ src, dst, fs string }{
		{"devtmpfs", "/dev", "devtmpfs"}, {"proc", "/proc", "proc"}, {"sysfs", "/sys", "sysfs"}, {"tmpfs", "/run", "tmpfs"},
	} {
		// The kernel may already have mounted devtmpfs; everything else is mounted once here.
		if err := mountAt(m.src, m.dst, m.fs, 0, ""); err != nil && !errors.Is(err, syscall.EBUSY) {
			return fail(err)
		}
	}
	if err := os.MkdirAll(containerRunDir, 0o700); err != nil {
		return fail(err)
	}
	_ = exec.Command("/bin/busybox", "--install", "-s").Run()
	modprobe("virtio_blk")
	if err := waitForDevice(configDevice); err != nil {
		return fail(err)
	}
	raw := make([]byte, 1<<20)
	f, err := os.Open(configDevice)
	if err != nil {
		return fail(err)
	}
	n, _ := f.Read(raw)
	f.Close()
	creditSeed(raw[:n])
	config, err := parseContainerConfig(raw[:n])
	if err != nil {
		return fail(err)
	}
	if err = prepareRoot(config); err != nil {
		return fail(err)
	}
	if config.Hostname != "" {
		_ = syscall.Sethostname([]byte(config.Hostname))
	}
	ip := configureNetwork(config)
	if ip != "" {
		marker("ip", ip)
	}
	hostname := config.Hostname
	if hostname == "" {
		hostname = "veynodevm"
	}
	_ = os.WriteFile(containerRunDir+"/hostname", []byte(hostname+"\n"), 0o644)
	_ = os.WriteFile(containerRunDir+"/hosts", []byte("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n127.0.1.1\t"+hostname+"\n"), 0o644)
	if len(config.DNS) > 0 {
		// Explicit servers replace the DHCP-provided gateway forwarder.
		var resolv strings.Builder
		for _, server := range config.DNS {
			resolv.WriteString("nameserver " + server + "\n")
		}
		_ = os.WriteFile(containerRunDir+"/resolv.conf", []byte(resolv.String()), 0o644)
	} else if _, err := os.Stat(containerRunDir + "/resolv.conf"); err != nil {
		_ = os.WriteFile(containerRunDir+"/resolv.conf", []byte("nameserver 1.1.1.1\n"), 0o644)
	}
	for _, name := range []string{"resolv.conf", "hosts", "hostname"} {
		bindFile(containerRunDir+"/"+name, filepath.Join(containerRootDir, "etc", name))
	}
	if config.GPU {
		if _, err := os.Stat("/dev/dri/renderD128"); err == nil {
			marker("gpu", "/dev/dri/renderD128")
			if config.Engine {
				_ = os.MkdirAll(filepath.Join(containerRootDir, "etc/cdi"), 0o755)
				_ = os.WriteFile(filepath.Join(containerRootDir, "etc/cdi/veynodevm.yaml"), []byte(cdiSpec), 0o644)
			}
		} else {
			marker("gpu", "unavailable")
		}
	}
	if config.Init != "systemd" {
		if err := writeModuleBlacklist(containerRootDir); err != nil {
			marker("warning", "module blacklist: "+err.Error())
		}
	}
	if config.Init == "systemd" {
		// Returns only on failure; on success the image's init is PID 1.
		return fail(handOffToSystemd(config))
	}
	uid, gid, home, err := lookupUser(containerRootDir, config.User)
	if err != nil {
		return fail(err)
	}
	env := withDefaultEnv(config.Env, home)
	envJSON, _ := json.Marshal(containerEnvFile{Env: env, Workdir: config.Workdir})
	_ = os.WriteFile(containerRunDir+"/env.json", envJSON, 0o600)
	agentArgs := []string{"--allow-execute", "--container-root", containerRootDir, "--container-env", containerRunDir + "/env.json"}
	if config.Transport == "virtio-serial-exec" {
		linkVirtioPorts()
		agentArgs = append([]string{"--virtio-serial-exec"}, agentArgs...)
	} else {
		agentArgs = append(agentArgs, "--allow-shutdown")
		if config.Desktop {
			// Apple Virtualization has no host capture for Linux; the browser
			// desktop reads the scanout through the agent.
			agentArgs = append(agentArgs, "--desktop-stream")
		}
	}
	agentLog, _ := os.OpenFile(containerRunDir+"/agent.log", os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	agent := exec.Command("/veynode/agent", agentArgs...)
	agent.Stdout, agent.Stderr = agentLog, agentLog
	if err = agent.Start(); err != nil {
		marker("warning", "guest service: "+err.Error())
	}
	identity := containerIdentity(containerRootDir, env, config.Workdir)
	executable, err := identity.executable(config.Argv[0])
	if err != nil {
		return fail(fmt.Errorf("%s: %w", config.Argv[0], err))
	}
	entry := exec.Command(executable, config.Argv[1:]...)
	entry.Env = env
	entry.Dir = config.Workdir
	if entry.Dir == "" {
		entry.Dir = "/"
	}
	entry.SysProcAttr = &syscall.SysProcAttr{Chroot: containerRootDir, Setsid: true}
	if uid != 0 || gid != 0 {
		entry.SysProcAttr.Credential = &syscall.Credential{Uid: uid, Gid: gid}
	}
	entry.Stdin, entry.Stdout, entry.Stderr = nil, os.Stdout, os.Stderr
	// With -t the entrypoint gets a pty, with -i an open stdin pipe; the
	// session server holds the other end for `attach`.
	var handover, childEnd *os.File
	mode := ""
	if config.TTY {
		if handover, childEnd, err = openPTY(containerRootDir); err != nil {
			return fail(fmt.Errorf("pty: %w", err))
		}
		entry.Stdin, entry.Stdout, entry.Stderr = childEnd, childEnd, childEnd
		entry.SysProcAttr.Setctty = true
		mode = "tty"
	} else if config.Stdin {
		if childEnd, handover, err = os.Pipe(); err != nil {
			return fail(err)
		}
		entry.Stdin = childEnd
		mode = "stdin"
	}
	signals := make(chan os.Signal, 4)
	signal.Notify(signals, syscall.SIGTERM, syscall.SIGINT, syscall.SIGUSR1, syscall.SIGUSR2)
	if err = entry.Start(); err != nil {
		return fail(err)
	}
	if childEnd != nil {
		childEnd.Close()
	}
	startSessionServer(config, agentLog, handover, mode)
	marker("started", strconv.Itoa(entry.Process.Pid))
	code := reapUntil(entry.Process.Pid, agent, signals)
	marker("exit", strconv.Itoa(code))
	powerOff(code)
	return code
}

// startSessionServer runs the interactive session service (session_linux.go)
// as its own process, so PID 1's reaper never takes its children's status.
func startSessionServer(config containerConfig, log *os.File, handover *os.File, mode string) {
	if config.SessionToken == "" {
		if handover != nil {
			handover.Close()
		}
		return
	}
	server := exec.Command("/veynode/agent", "session-server", containerRootDir, containerRunDir+"/env.json")
	server.Env = []string{"VEYNODEVM_SESSION_TOKEN=" + config.SessionToken}
	if handover != nil {
		server.Env = append(server.Env, "VEYNODEVM_MAIN_STDIO="+mode)
		server.ExtraFiles = []*os.File{handover}
	}
	// The entrypoint's terminal output goes to the console from there.
	server.Stdout, server.Stderr = os.Stdout, log
	if err := server.Start(); err != nil {
		marker("warning", "session server: "+err.Error())
	}
	if handover != nil {
		handover.Close()
	}
}

// reapUntil is the PID 1 loop: it reaps every orphan and returns the main
// process status. Stop signals are forwarded with a 10 second grace period.
func reapUntil(pid int, agent *exec.Cmd, signals chan os.Signal) int {
	exited := make(chan int, 1)
	go func() {
		for {
			var status syscall.WaitStatus
			child, err := syscall.Wait4(-1, &status, 0, nil)
			if err == syscall.EINTR {
				continue
			}
			if err != nil {
				time.Sleep(50 * time.Millisecond)
				continue
			}
			if child == pid {
				if status.Signaled() {
					exited <- 128 + int(status.Signal())
				} else {
					exited <- status.ExitStatus()
				}
				return
			}
		}
	}()
	var kill <-chan time.Time
	for {
		select {
		case code := <-exited:
			return code
		case <-signals:
			_ = syscall.Kill(-pid, syscall.SIGTERM)
			_ = syscall.Kill(pid, syscall.SIGTERM)
			kill = time.After(10 * time.Second)
		case <-kill:
			_ = syscall.Kill(-pid, syscall.SIGKILL)
			_ = syscall.Kill(pid, syscall.SIGKILL)
		}
	}
}

func powerOff(code int) {
	_ = syscall.Kill(-1, syscall.SIGTERM)
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		var status syscall.WaitStatus
		if _, err := syscall.Wait4(-1, &status, syscall.WNOHANG, nil); err == syscall.ECHILD {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	_ = syscall.Kill(-1, syscall.SIGKILL)
	syscall.Sync()
	if mounts, err := os.ReadFile("/proc/self/mounts"); err == nil {
		var targets []string
		scanner := bufio.NewScanner(bytes.NewReader(mounts))
		for scanner.Scan() {
			fields := strings.Fields(scanner.Text())
			if len(fields) > 1 && (strings.HasPrefix(fields[1], containerRootDir) || fields[1] == "/run/disk") {
				targets = append(targets, fields[1])
			}
		}
		for i := len(targets) - 1; i >= 0; i-- {
			_ = syscall.Unmount(targets[i], syscall.MNT_DETACH)
		}
	}
	syscall.Sync()
	_ = code
	_ = syscall.Reboot(syscall.LINUX_REBOOT_CMD_POWER_OFF)
}

// linkVirtioPorts creates the /dev/virtio-ports/<name> links udev would make;
// container-init runs without udev, so devtmpfs only has /dev/vportNpM.
func linkVirtioPorts() {
	ports, _ := filepath.Glob("/sys/class/virtio-ports/vport*")
	if len(ports) == 0 {
		return
	}
	_ = os.MkdirAll("/dev/virtio-ports", 0o755)
	for _, port := range ports {
		name, err := os.ReadFile(port + "/name")
		trimmed := strings.TrimSpace(string(name))
		if err != nil || trimmed == "" || strings.ContainsRune(trimmed, '/') {
			continue
		}
		_ = os.Symlink("../"+filepath.Base(port), "/dev/virtio-ports/"+trimmed)
	}
}
