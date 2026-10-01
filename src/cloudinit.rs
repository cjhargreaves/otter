use crate::schema::App;

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn render(app: &App) -> String {
    let mut script = String::new();
    script.push_str("#!/bin/bash\nset -eux\n");
    script.push_str("dnf install -y docker\n");
    script.push_str("systemctl enable --now docker\n");

    let mut run_args = vec![
        "docker run -d --restart always --name app".to_string(),
        format!("-p {}:{}", app.port, app.port),
    ];

    if let Some(volume) = &app.volume {
        script.push_str("DEV=\"\"\n");
        script.push_str("for attempt in $(seq 1 90); do\n");
        script.push_str("  for candidate in /dev/nvme1n1 /dev/xvdf /dev/sdf; do\n");
        script.push_str("    if [ -b \"$candidate\" ]; then DEV=\"$candidate\"; break 2; fi\n");
        script.push_str("  done\n");
        script.push_str("  sleep 2\n");
        script.push_str("done\n");
        script.push_str("if [ -z \"$DEV\" ]; then echo 'volume never attached' >&2; exit 1; fi\n");
        script.push_str("if ! blkid \"$DEV\"; then mkfs.ext4 \"$DEV\"; fi\n");
        script.push_str("mkdir -p /otter/data\n");
        script.push_str("mount \"$DEV\" /otter/data\n");
        run_args.push(format!("-v /otter/data:{}", shell_quote(&volume.mount)));
    }

    for (key, value) in app.container_env() {
        run_args.push(format!("-e {}={}", key, shell_quote(&value)));
    }
    run_args.push(shell_quote(&app.image));

    script.push_str(&run_args.join(" \\\n  "));
    script.push('\n');
    script
}
