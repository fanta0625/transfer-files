//! Desktop file selection; only paths on the client can be selected here.
pub fn choose_private_key() -> Result<Option<String>, String> {
    #[cfg(target_os = "linux")]
    {
        use std::process::Command;
        for (program, args) in [
            (
                "zenity",
                vec![
                    "--file-selection",
                    "--title=选择 SSH 私钥文件（不是公钥 .pub）",
                ],
            ),
            (
                "kdialog",
                vec![
                    "--getopenfilename",
                    ".",
                    "*",
                    "--title",
                    "选择 SSH 私钥文件",
                ],
            ),
        ] {
            let output = match Command::new(program).args(args).output() {
                Ok(output) => output,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("无法打开文件选择器：{error}")),
            };
            if output.status.code() == Some(1) {
                return Ok(None);
            }
            if !output.status.success() {
                return Err("无法打开文件选择器，请检查桌面会话；也可手动填写私钥路径".into());
            }
            let path = String::from_utf8(output.stdout)
                .map_err(|_| "文件路径不是有效 UTF-8".to_string())?;
            let path = path.trim_end_matches(['\r', '\n']);
            return if path.is_empty() {
                Ok(None)
            } else {
                Ok(Some(path.into()))
            };
        }
        Err("请安装 zenity 或 kdialog 以选择文件，也可手动填写私钥路径".into())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("文件选择目前支持 Linux 客户端；请手动填写私钥路径".into())
    }
}
