//! Shell-specific SSH launchers. No passwords are included in commands.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]
use crate::config::{RemoteOs, SshNode};
use std::io::Write;

pub(crate) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn ps_command(script: &str) -> String {
    use base64::Engine as _;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder
        .write_all(script.as_bytes())
        .expect("memory compression");
    let data = base64::engine::general_purpose::STANDARD
        .encode(encoder.finish().expect("memory compression"));
    // No $ interpolation in the bootstrap: safe with both cmd.exe and PowerShell default shells.
    format!(
        "powershell.exe -NoLogo -NoProfile -Command \"Invoke-Expression (New-Object IO.StreamReader((New-Object IO.Compression.GzipStream((New-Object IO.MemoryStream(,[Convert]::FromBase64String('{data}'))),[IO.Compression.CompressionMode]::Decompress)),[Text.Encoding]::UTF8)).ReadToEnd()\""
    )
}

pub(crate) fn identity_args(node: &SshNode) -> Vec<String> {
    if node.options.identity_file.is_empty() {
        Vec::new()
    } else {
        vec![
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-i".into(),
            node.options.identity_file.clone(),
        ]
    }
}
/// Windows native argv quoting (PowerShell 5.1's `& ... @args` loses embedded quotes).
pub(crate) fn windows_arg(arg: &str) -> String {
    let mut result = String::from("\"");
    let mut slashes = 0;
    for ch in arg.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        if ch == '"' {
            result.push_str(&"\\".repeat(slashes * 2 + 1));
        } else {
            result.push_str(&"\\".repeat(slashes));
        }
        slashes = 0;
        result.push(ch);
    }
    result.push_str(&"\\".repeat(slashes * 2));
    result.push('"');
    result
}
pub(crate) fn ps_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
pub(crate) fn invoke(os: RemoteOs, args: &[String], marker: &str) -> String {
    match os {
        RemoteOs::Linux => format!(
            "stty -echo; printf '%s\\n' {}; exec env LC_ALL=C SSH_ASKPASS_REQUIRE=never {}",
            quote(marker),
            args.iter().map(|v| quote(v)).collect::<Vec<_>>().join(" ")
        ),
        RemoteOs::Windows => ps_command(&format!(
            "$ErrorActionPreference='Stop'; $env:LC_ALL='C'; $env:SSH_ASKPASS_REQUIRE='never'; [Console]::WriteLine({}); $p=New-Object Diagnostics.ProcessStartInfo; $p.FileName={}; $p.Arguments={}; $p.UseShellExecute=$false; $c=[Diagnostics.Process]::Start($p); $c.WaitForExit(); exit $c.ExitCode",
            ps_literal(marker),
            ps_literal(&args[0]),
            ps_literal(
                &args[1..]
                    .iter()
                    .map(|s| windows_arg(s))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_and_native_arguments() {
        let mut node = SshNode::default();
        assert!(identity_args(&node).is_empty());
        node.options.identity_file = "C:/Users/a b/key".into();
        assert_eq!(
            identity_args(&node),
            ["-o", "IdentitiesOnly=yes", "-i", "C:/Users/a b/key"]
        );
        assert_eq!(windows_arg("a\"b\\"), "\"a\\\"b\\\\\"");
        assert_eq!(quote("a'b"), "'a'\\''b'");
    }
    #[cfg(windows)]
    #[test]
    fn windows_bootstrap_and_nested_launcher_work_in_both_default_shells() {
        use std::{os::windows::process::CommandExt, process::Command};
        let temp = tempfile::tempdir().unwrap();
        let script_path = temp.path().join("native arguments.ps1");
        std::fs::write(&script_path, "param([string]$value) [Console]::WriteLine([Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($value)))").unwrap();
        let value = "user name ' 中文 & $(exit 81)";
        let args = vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-File".into(),
            script_path.display().to_string(),
            value.into(),
        ];
        let command = invoke(RemoteOs::Windows, &args, "test-marker");
        for cmd_shell in [true, false] {
            let output = if cmd_shell {
                Command::new("cmd.exe")
                    .args(["/d", "/c"])
                    .raw_arg(&command)
                    .output()
                    .unwrap()
            } else {
                Command::new("powershell.exe")
                    .args(["-NoProfile", "-Command", &command])
                    .output()
                    .unwrap()
            };
            use base64::Engine as _;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("test-marker"));
            assert!(
                text.contains(&base64::engine::general_purpose::STANDARD.encode(value)),
                "{text}"
            );
        }
        let request = serde_json::json!({"op":"manifest", "path":script_path});
        let command = ps_command(&crate::remote::operations::script(
            RemoteOs::Windows,
            &request,
            "test",
        ));
        assert!(command.len() < 8191);
        let output = Command::new("cmd.exe")
            .args(["/d", "/c"])
            .raw_arg(command)
            .output()
            .unwrap();
        let manifest =
            crate::remote::operations::parse(&String::from_utf8_lossy(&output.stdout), "test")
                .unwrap();
        assert_eq!(manifest.as_array().unwrap().len(), 1);
    }
}
