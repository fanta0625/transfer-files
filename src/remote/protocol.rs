use super::Listing;
use crate::files::Entry;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::io;

pub fn encode_utf16le(value: &str) -> String {
    STANDARD.encode(
        value
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}

pub fn script(path: &str, token: &str) -> String {
    let path = path.replace('\'', "''");
    format!(
        r#"$ErrorActionPreference='Stop';
[Console]::OutputEncoding=[Text.Encoding]::ASCII;
function B64([string]$value) {{ [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($value)) }}
try {{
  $item=Get-Item -LiteralPath '{path}' -Force -ErrorAction Stop;
  if (-not $item.PSIsContainer) {{ throw 'The requested path is not a directory' }}
  $items=@(Get-ChildItem -LiteralPath $item.FullName -Force -ErrorAction Stop);
  Write-Output 'TF_LIST_V1_BEGIN_{token}';
  Write-Output ('P' + "`t" + (B64 $item.FullName));
  foreach ($entry in $items) {{
    $link=($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0;
    $kind=if ($link) {{ 'L' }} elseif ($entry.PSIsContainer) {{ 'D' }} else {{ 'F' }};
    $size=if ($entry.PSIsContainer -or $link) {{ 0 }} else {{ $entry.Length }};
    Write-Output ($kind + "`t" + $size + "`t" + (B64 $entry.LastWriteTimeUtc.ToString('yyyy-MM-dd HH:mm UTC')) + "`t" + (B64 $entry.Name) + "`t" + (B64 $entry.FullName));
  }}
  Write-Output 'TF_LIST_V1_END_{token}';
  exit 0;
}} catch {{
  Write-Output ('TF_ERROR_{token}' + "`t" + (B64 $_.Exception.Message));
  exit 1;
}}"#
    )
}

pub fn parse(output: &str, token: &str) -> io::Result<Listing> {
    let begin = format!("TF_LIST_V1_BEGIN_{token}");
    let end = format!("TF_LIST_V1_END_{token}");
    let error = format!("TF_ERROR_{token}\t");
    let mut inside = false;
    let mut complete = false;
    let mut path = None;
    let mut entries = Vec::new();
    for line in output.lines().map(|line| line.trim_end_matches('\r')) {
        if let Some(message) = line.strip_prefix(&error) {
            return Err(io::Error::other(format!("Windows：{}", decode(message)?)));
        }
        if line == begin {
            if inside || complete {
                return Err(invalid("远端重复返回目录数据"));
            }
            inside = true;
            continue;
        }
        if line == end {
            if !inside {
                return Err(invalid("远端目录数据顺序错误"));
            }
            complete = true;
            inside = false;
            continue;
        }
        if !inside {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        match fields.as_slice() {
            ["P", value] if path.is_none() => path = Some(decode(value)?),
            [kind @ ("D" | "F" | "L"), size, modified, name, full_path] if path.is_some() => {
                entries.push(Entry {
                    path: decode(full_path)?.into(),
                    name: decode(name)?,
                    directory: *kind == "D",
                    link: *kind == "L",
                    size: size.parse().map_err(|_| invalid("远端大小无效"))?,
                    modified: decode(modified)?,
                });
            }
            _ => return Err(invalid("远端目录数据格式错误")),
        }
    }
    let path = path
        .filter(|p| !p.is_empty())
        .ok_or_else(|| invalid("远端未返回目录；检查 Windows SSH 与 PowerShell"))?;
    if !complete || inside {
        return Err(invalid("远端目录返回中断，未更新列表"));
    }
    entries.sort_by(|a, b| {
        b.directory
            .cmp(&a.directory)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok((path.into(), entries))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn decode(value: &str) -> io::Result<String> {
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| invalid("远端字段不是有效 Base64"))?;
    String::from_utf8(bytes).map_err(|_| invalid("远端字段不是 UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framed_unicode_listing_is_complete_and_directories_first() {
        let b = |s: &str| STANDARD.encode(s);
        let output = format!(
            "banner\nTF_LIST_V1_BEGIN_test\nP\t{}\nF\t4\t{}\t{}\t{}\nD\t0\t{}\t{}\t{}\nTF_LIST_V1_END_test\nclosed\n",
            b("C:/资料"),
            b("date"),
            b("行\n文\t件.txt"),
            b("C:/资料/行\n文\t件.txt"),
            b("date"),
            b("目录"),
            b("C:/资料/目录")
        );
        let (path, entries) = parse(&output, "test").unwrap();
        assert_eq!(path.to_str(), Some("C:/资料"));
        assert_eq!(entries[0].name, "目录");
        assert_eq!(entries[1].name, "行\n文\t件.txt");
        assert!(parse(&output.replace("TF_LIST_V1_END_test", ""), "test").is_err());
        assert!(parse(&output.replace("F\t4", "F\tbad"), "test").is_err());
        let empty = format!(
            "TF_LIST_V1_BEGIN_test\nP\t{}\nTF_LIST_V1_END_test\n",
            b("C:/empty")
        );
        assert!(parse(&empty, "test").unwrap().1.is_empty());
        assert!(
            parse(&format!("TF_ERROR_test\t{}\n", b("Access denied")), "test")
                .unwrap_err()
                .to_string()
                .contains("Access denied")
        );
    }
    #[test]
    fn script_quotes_paths_and_encodes_all_unicode_as_utf16() {
        let source = script("C:/资料 ' ; $(whoami)", "test");
        assert!(source.contains("C:/资料 '' ; $(whoami)"));
        let bytes = STANDARD.decode(encode_utf16le(&source)).unwrap();
        let units: Vec<_> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), source);
    }
    #[cfg(windows)]
    #[test]
    fn powershell_reads_real_unicode_and_empty_directories() {
        use std::process::Command;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("资料 ' $(whoami)");
        std::fs::create_dir_all(directory.join("空目录")).unwrap();
        std::fs::write(directory.join("内容.txt"), b"test").unwrap();
        let run = |path: &std::path::Path| {
            Command::new("powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-EncodedCommand",
                    &encode_utf16le(&script(path.to_str().unwrap(), "test")),
                ])
                .output()
                .unwrap()
        };
        let output = run(&directory);
        assert!(output.status.success(), "{:?}", output.stderr);
        let (_, entries) = parse(&String::from_utf8(output.stdout).unwrap(), "test").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "空目录");
        assert!(
            parse(
                &String::from_utf8(run(&directory.join("空目录")).stdout).unwrap(),
                "test"
            )
            .unwrap()
            .1
            .is_empty()
        );
        assert!(!run(&directory.join("内容.txt")).status.success());
    }
}
