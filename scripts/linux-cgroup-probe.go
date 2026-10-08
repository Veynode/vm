//go:build linux

// Real kernel capability probe, compiled for the disposable Linux Guest only.
package main

import (
 "fmt"
 "os"
 "os/exec"
 "path/filepath"
 "strings"
 "syscall"
 "time"
)
func must(err error) { if err != nil { panic(err) } }
func main() {
 if len(os.Args)>1 {
  switch os.Args[1] {
  case "grandchild": time.Sleep(1200*time.Millisecond);must(os.WriteFile("/tmp/cgroup-marker",[]byte("escaped"),0600));return
  case "sidecar": time.Sleep(10*time.Second);return
  case "child":
   data,err:=os.ReadFile("/proc/self/cgroup");must(err);fmt.Printf("CGROUP_CHILD_MEMBERSHIP %s",data)
   if !strings.Contains(string(data),"/veynode-probe/job") { panic("not born in expected group") }
   err=os.WriteFile("/sys/fs/cgroup/veynode-probe/cgroup.procs",[]byte(fmt.Sprint(os.Getpid())),0600)
   if err==nil {panic("nonroot child could migrate out")};fmt.Println("CGROUP_MIGRATION_DENIED",err)
   detached:=exec.Command(os.Args[0],"grandchild");detached.SysProcAttr=&syscall.SysProcAttr{Setsid:true};must(detached.Start());return
  }
 }
 const root="/sys/fs/cgroup/veynode-probe"
 must(os.Mkdir(root,0700));defer os.Remove(root)
 leaf:=filepath.Join(root,"job");must(os.Mkdir(leaf,0700));defer os.Remove(leaf)
 dir,err:=os.Open(leaf);must(err);defer dir.Close()
 sidecar:=exec.Command(os.Args[0],"sidecar");sidecar.SysProcAttr=&syscall.SysProcAttr{Credential:&syscall.Credential{Uid:1001,Gid:1001,Groups:[]uint32{}}};must(sidecar.Start());defer func(){sidecar.Process.Kill();sidecar.Wait()}()
 child:=exec.Command(os.Args[0],"child");child.Stdout=os.Stdout;child.Stderr=os.Stderr
 child.SysProcAttr=&syscall.SysProcAttr{UseCgroupFD:true,CgroupFD:int(dir.Fd()),Setpgid:true,Credential:&syscall.Credential{Uid:1000,Gid:1000,Groups:[]uint32{}}}
 must(child.Run());fmt.Println("CGROUP_CLONE3_ATOMIC_EXEC_PASS")
 data,err:=os.ReadFile(leaf+"/cgroup.events");must(err);fmt.Printf("CGROUP_BEFORE_KILL %s",data)
 must(os.WriteFile(leaf+"/cgroup.kill",[]byte("1"),0600))
 deadline:=time.Now().Add(time.Second)
 for {data,err=os.ReadFile(leaf+"/cgroup.events");must(err);if strings.Contains(string(data),"populated 0"){break};if time.Now().After(deadline){panic("group not empty")};time.Sleep(time.Millisecond)}
 fmt.Printf("CGROUP_AFTER_KILL %s",data);must(sidecar.Process.Signal(syscall.Signal(0)))
 fmt.Println("CGROUP_SEPARATE_UID1001_SIDECAR_ALIVE")
 time.Sleep(1400*time.Millisecond)
 if _,err=os.Stat("/tmp/cgroup-marker");!os.IsNotExist(err){panic("detached descendant survived")}
 fmt.Println("CGROUP_CAPABILITY_PROBE_PASS")
}
