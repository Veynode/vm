//! QEMU sound card for `Config::audio`. The card is backed by SPICE audio, so
//! the SPICE console client (the VM's only SPICE consumer) receives playback
//! PCM and feeds microphone PCM, and relays both to whichever viewer is open.
use crate::Config;

/// Arguments after `-spice`; empty when audio is off. Linux uses virtio-sound
/// (Alpine's virt kernel ships `virtio_snd`); Windows has an in-box HD Audio
/// driver but none for virtio-sound.
pub fn arguments(config: &Config) -> Vec<String> {
    if !config.audio {
        return Vec::new();
    }
    let mut args = vec!["-audiodev".into(), "spice,id=veynode-audio".into()];
    if config.guest == "windows" {
        args.extend([
            "-device".into(),
            "ich9-intel-hda,id=veynode-hda".into(),
            "-device".into(),
            "hda-duplex,bus=veynode-hda.0,audiodev=veynode-audio".into(),
        ]);
    } else {
        args.extend([
            "-device".into(),
            "virtio-sound-pci,audiodev=veynode-audio,streams=2".into(),
        ]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(guest: &str, audio: bool) -> Config {
        serde_json::from_value(serde_json::json!({
            "backend":"qemu-hvf","id":"c1","guest":guest,"cpus":1,"memory_mib":512,
            "kernel":"/k","audio":audio
        }))
        .unwrap()
    }
    #[test]
    fn no_sound_card_without_audio() {
        assert!(arguments(&config("linux", false)).is_empty());
    }
    #[test]
    fn linux_gets_virtio_sound_on_spice_audio() {
        assert_eq!(
            arguments(&config("linux", true)),
            [
                "-audiodev",
                "spice,id=veynode-audio",
                "-device",
                "virtio-sound-pci,audiodev=veynode-audio,streams=2"
            ]
        );
    }
    #[test]
    fn windows_gets_hd_audio() {
        let args = arguments(&config("windows", true));
        assert_eq!(args[3], "ich9-intel-hda,id=veynode-hda");
        assert_eq!(
            args[5],
            "hda-duplex,bus=veynode-hda.0,audiodev=veynode-audio"
        );
    }
    #[test]
    fn macos_guests_take_audio_on_apple() {
        // Their Guest service records a sound card the host discards.
        let dir = std::env::temp_dir().join(format!("vvm-macos-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("disk.img"), b"").unwrap();
        let c: Config = serde_json::from_value(serde_json::json!({
            "backend":"apple-virtualization","id":"c1","guest":"macos","cpus":4,"memory_mib":8192,
            "disk":dir.join("disk.img"),"machine_identity_dir":dir,"audio":true
        }))
        .unwrap();
        let checked = c.validate();
        std::fs::remove_dir_all(&dir).unwrap();
        checked.unwrap();
    }
}
