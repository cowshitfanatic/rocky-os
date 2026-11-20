use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let bios_path = env!("BIOS_PATH");

    // Data disk for RockyFS: 8 MiB raw image, created once and
    // persisted across boots so files survive a reboot.
    let disk_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("disk.img");
    if !disk_path.exists() {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&disk_path)
            .expect("create disk.img");
        f.write_all(&[0u8; 1024]).expect("write disk.img");
        f.set_len(8 * 1024 * 1024).expect("size disk.img");
        println!("created data disk: {}", disk_path.display());
    }

    let mut cmd = Command::new("qemu-system-x86_64");

    cmd.arg("-drive").arg(format!("format=raw,file={bios_path}"));
    cmd.arg("-drive").arg(format!(
        "format=raw,file={},if=ide,index=1,media=disk",
        disk_path.display()
    ));
    // Serial on stdio: the host terminal/stdin feeds COM1, so the shell
    // is scriptable (`printf 'help\nexit\n' | cargo run`). The monitor
    // socket is for `sendkey` (PS/2 keyboard) and `screendump` tests.
    cmd.arg("-serial").arg("stdio");
    // COM2: second serial port for the browser's net proxy
    // (scripts/net-proxy.py). `server,nowait` so booting never
    // blocks when no proxy is connected.
    cmd.arg("-serial").arg("unix:/tmp/rocky-net,server,nowait");
    cmd.arg("-display").arg("none");
    cmd.arg("-monitor")
        .arg("unix:/tmp/rocky-mon,server,nowait");
    cmd.arg("-device").arg("isa-debug-exit,iobase=0xf4,iosize=0x04");

    let status = cmd
        .spawn()
        .expect("failed to start qemu-system-x86_64")
        .wait()
        .unwrap();

    println!("QEMU exited with: {status}");
}
