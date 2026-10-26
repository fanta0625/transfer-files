//! Short-lived filesystem helpers; nothing is installed on remote computers.
use crate::config::RemoteOs;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::io;

pub fn script(os: RemoteOs, request: &Value, token: &str) -> String {
    let request = STANDARD.encode(serde_json::to_vec(request).unwrap());
    match os {
        RemoteOs::Linux => format!(
            "import json,base64\nREQUEST=json.loads(base64.b64decode('{request}'))\nTOKEN={}\nprint('TF_READY_'+TOKEN, flush=True)\n{}",
            serde_json::to_string(token).unwrap(),
            include_str!("fs_helper.py")
        ),
        RemoteOs::Windows => format!(
            "$request=ConvertFrom-Json ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{request}'))); $token='{}'; [Console]::WriteLine('TF_READY_' + $token);\n{}",
            token.replace('\'', "''"),
            include_str!("fs_helper.ps1")
        ),
    }
}

#[cfg(target_os = "linux")]
pub fn command(os: RemoteOs, request: &Value, token: &str) -> String {
    match os {
        RemoteOs::Linux => format!(
            "python3 -c {}",
            super::terminal::quote(&script(os, request, token))
        ),
        RemoteOs::Windows => super::shell::ps_command(&script(os, request, token)),
    }
}

pub fn parse(output: &str, token: &str) -> io::Result<Value> {
    let begin = format!("TF_RPC_{token}_BEGIN");
    let end = format!("TF_RPC_{token}_END");
    let mut inside = false;
    let mut complete = false;
    let mut data = String::new();
    for line in output.lines().map(str::trim) {
        if line == begin {
            if inside || complete {
                return Err(io::Error::other("重复的远端响应"));
            }
            inside = true;
        } else if line == end {
            if !inside {
                return Err(io::Error::other("远端响应顺序无效"));
            }
            complete = true;
            inside = false;
        } else if inside {
            data.push_str(line);
        }
    }
    if !complete || inside {
        return Err(io::Error::other(
            "远端响应不完整；Linux 需要 Python 3，Windows 需要 PowerShell 5.1",
        ));
    }
    let bytes = STANDARD.decode(data).map_err(io::Error::other)?;
    let answer: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if answer["ok"] != true {
        return Err(io::Error::other(
            answer["error"]
                .as_str()
                .unwrap_or("远端操作失败")
                .to_string(),
        ));
    }
    Ok(answer["value"].clone())
}

#[cfg(target_os = "linux")]
pub fn listing(output: &str, token: &str) -> io::Result<super::Listing> {
    let value = parse(output, token)?;
    let path = value["path"]
        .as_str()
        .ok_or_else(|| io::Error::other("目录路径无效"))?;
    let mut entries: Vec<crate::files::Entry> =
        serde_json::from_value(value["entries"].clone()).map_err(io::Error::other)?;
    entries.sort_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok((path.into(), entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Command;

    fn execute(request: Value) -> io::Result<Value> {
        #[cfg(windows)]
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-EncodedCommand",
                &super::super::protocol::encode_utf16le(&script(
                    RemoteOs::Windows,
                    &request,
                    "test",
                )),
            ])
            .output()?;
        #[cfg(not(windows))]
        let output = Command::new("python3")
            .args(["-c", &script(RemoteOs::Linux, &request, "test")])
            .output()?;
        parse(&String::from_utf8_lossy(&output.stdout), "test")
    }

    #[test]
    fn manifests_hashes_and_no_replace_publication() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("资料 ' $(whoami)");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(src.join("empty")).unwrap();
        std::fs::write(src.join("file.txt"), "abcdef").unwrap();
        let listing = execute(json!({"op":"list", "path":src})).unwrap();
        assert_eq!(listing["entries"].as_array().unwrap().len(), 2);
        let rows = execute(json!({"op":"manifest", "path":src})).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 3);
        let digest = execute(json!({"op":"hash", "path":src.join("file.txt"), "limit":3})).unwrap();
        assert_eq!(
            digest["hash"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let destination = root.path().join("exists");
        std::fs::write(&destination, "keep").unwrap();
        assert!(
            execute(json!({"op":"publish", "source":src.join("file.txt"), "target":destination}))
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), "keep");
        let destination = root.path().join("published");
        execute(json!({"op":"publish", "source":src, "target":destination})).unwrap();
        assert!(destination.join("empty").is_dir());
    }

    #[test]
    fn private_staging_is_reused_and_cleaned() {
        let root = tempfile::tempdir().unwrap();
        let request = json!({"op":"prepare", "base":root.path(), "id":"test-staging"});
        let first = execute(request.clone()).unwrap();
        assert_eq!(execute(request).unwrap(), first);
        let path = first.as_str().unwrap();
        let payload = format!("{path}/payload/empty");
        execute(json!({"op":"mkdir", "path":payload})).unwrap();
        let manifest = execute(json!({"op":"manifest", "path":payload})).unwrap();
        assert_eq!(manifest.as_array().unwrap().len(), 1);
        execute(json!({"op":"remove", "path":path})).unwrap();
        assert!(!std::path::Path::new(path).exists());
    }

    #[test]
    fn rejects_truncated_or_duplicate_responses() {
        let data = STANDARD.encode(br#"{"ok":true,"value":123}"#);
        let output = format!("banner\nTF_RPC_test_BEGIN\n{data}\nTF_RPC_test_END\n");
        assert_eq!(parse(&output, "test").unwrap(), 123);
        assert!(parse(&output.replace("TF_RPC_test_END", ""), "test").is_err());
        assert!(parse(&(output.clone() + &output), "test").is_err());
    }
}
