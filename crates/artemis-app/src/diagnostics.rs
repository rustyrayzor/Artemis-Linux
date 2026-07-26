use std::fs;
use std::path::Path;
use std::process::Command;

pub fn print_report() {
    println!("Artemis Linux beta diagnostics");
    println!("version={}", env!("CARGO_PKG_VERSION"));
    println!("target_os={}", std::env::consts::OS);
    println!("target_arch={}", std::env::consts::ARCH);
    report_environment("XDG_SESSION_TYPE");
    report_environment("XDG_CURRENT_DESKTOP");
    report_environment("WAYLAND_DISPLAY");
    report_environment("DISPLAY");
    report_file("os_release", Path::new("/etc/os-release"));
    report_command("kernel", "uname", &["-a"]);
    report_command("graphics", "lspci", &["-nnk"]);
    report_command("gstreamer", "gst-inspect-1.0", &["--version"]);
    for plugin in [
        "h264parse",
        "avdec_h264",
        "opusparse",
        "opusdec",
        "pipewiresink",
    ] {
        report_plugin(plugin);
    }
    println!(
        "dri_render_nodes={}",
        directory_entries(Path::new("/dev/dri")).join(",")
    );
}

fn report_environment(name: &str) {
    println!(
        "{}={}",
        name.to_ascii_lowercase(),
        std::env::var(name).unwrap_or_else(|_| "unset".to_owned())
    );
}

fn report_file(label: &str, path: &Path) {
    let value = fs::read_to_string(path).map_or_else(
        |error| format!("unavailable:{error}"),
        |text| text.lines().collect::<Vec<_>>().join(";"),
    );
    println!("{label}={value}");
}

fn report_command(label: &str, command: &str, arguments: &[&str]) {
    let value = Command::new(command).args(arguments).output().map_or_else(
        |error| format!("unavailable:{error}"),
        |output| {
            if output.status.success() {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .collect::<Vec<_>>()
                    .join(";")
            } else {
                format!("failed:{}", output.status)
            }
        },
    );
    println!("{label}={value}");
}

fn report_plugin(plugin: &str) {
    let available = Command::new("gst-inspect-1.0")
        .arg(plugin)
        .output()
        .is_ok_and(|output| output.status.success());
    println!("gstreamer_plugin_{plugin}={available}");
}

fn directory_entries(path: &Path) -> Vec<String> {
    fs::read_dir(path).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        },
    )
}
