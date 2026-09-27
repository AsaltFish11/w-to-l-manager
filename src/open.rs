//! 用系统默认程序打开链接。
//!
//! 刻意不用 egui 的 `links` feature：它会带进 `webbrowser` → `url` → ICU
//! 一整套依赖，而这里只需要调用系统自带的打开命令就够了。

use std::process::Command;

/// 校验链接：只允许 http / https，返回去掉首尾空白后的地址。
fn validate(url: &str) -> Result<&str, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("链接是空的".to_string());
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("只支持 http/https 链接：{url}"));
    }
    Ok(url)
}

/// 用系统默认浏览器打开一个 URL。
pub fn open_url(url: &str) -> Result<(), String> {
    let url = validate(url)?;

    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        // Windows 上 `start` 是 cmd 内置命令，第一个空参数是窗口标题
        ("cmd", vec!["/C", "start", "", url])
    } else {
        ("xdg-open", vec![url])
    };

    match Command::new(program).args(&args).spawn() {
        Ok(_) => {
            log::info!("已调用 {program} 打开 {url}");
            Ok(())
        }
        Err(err) => {
            let message = format!("无法调用 {program} 打开链接：{err}");
            log::warn!("{message}");
            Err(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_and_https_are_allowed() {
        assert_eq!(validate(" https://example.com ").unwrap(), "https://example.com");
        assert_eq!(validate("http://example.com").unwrap(), "http://example.com");

        assert!(validate("").is_err());
        assert!(validate("   ").is_err());
        // 别的协议一律拒绝，免得被 json 里随便一个字符串带偏
        assert!(validate("file:///etc/passwd").is_err());
        assert!(validate("javascript:alert(1)").is_err());
        assert!(validate("example.com").is_err());
    }
}
