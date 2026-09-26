//! 运行时从系统里找一款中文字体并注入 egui。
//!
//! egui 自带字体不含 CJK 字形，若不额外加载，中文说明会显示成方框。
//! 这里不把字体打包进二进制，而是按“fontconfig → 常见路径 → 目录扫描”的顺序
//! 在用户机器上现找一款可用字体。

use eframe::egui::{Context, FontData, FontDefinitions, FontFamily};
use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// 实际选中的字体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedFont {
    pub path: PathBuf,
    /// 字体集合（.ttc/.otc）中的第几个字面。
    pub index: u32,
}

impl LoadedFont {
    pub fn display_name(&self) -> String {
        let file = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("未知字体");
        if self.index == 0 {
            file.to_string()
        } else {
            format!("{file} (face {})", self.index)
        }
    }
}

/// 已知的中文字体路径，按优先级排列。
const KNOWN_FONTS: &[(&str, u32)] = &[
    ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 2),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 2),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-SC-Regular.otf", 0),
    ("/usr/share/fonts/noto-cjk/NotoSansCJK-SC-Regular.otf", 0),
    (
        "/usr/share/fonts/adobe-source-han-sans/SourceHanSansCN-Regular.otf",
        0,
    ),
    (
        "/usr/share/fonts/opentype/source-han-sans/SourceHanSansSC-Regular.otf",
        0,
    ),
    ("/usr/share/fonts/TTF/LXGWWenKaiScreen.ttf", 0),
    ("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
    ("/usr/share/fonts/wqy-microhei/wqy-microhei.ttc", 0),
    ("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 0),
    ("/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf", 0),
    ("/usr/share/fonts/truetype/arphic/uming.ttc", 0),
    ("/System/Library/Fonts/PingFang.ttc", 0),
    ("/System/Library/Fonts/Hiragino Sans GB.ttc", 0),
    ("/Library/Fonts/Arial Unicode.ttf", 0),
    (r"C:\Windows\Fonts\msyh.ttc", 0),
    (r"C:\Windows\Fonts\simhei.ttf", 0),
    (r"C:\Windows\Fonts\simsun.ttc", 0),
];

/// 目录扫描时用于打分的文件名关键字（越靠前越优先）。
const NAME_KEYWORDS: &[&str] = &[
    "notosanssc",
    "notosanscjk",
    "sourcehansanssc",
    "sourcehansanscn",
    "sourcehansans",
    "wqy-microhei",
    "wqy-zenhei",
    "lxgwwenkai",
    "droidsansfallback",
    "msyh",
    "simhei",
    "pingfang",
    "hiraginosansgb",
    "uming",
    "unicode",
];

const SCAN_DIRS: &[&str] = &[
    "/usr/share/fonts",
    "/usr/local/share/fonts",
    "/System/Library/Fonts",
    "/Library/Fonts",
    r"C:\Windows\Fonts",
];

/// 注入中文字体，返回被选中的字体信息。
pub fn install_cjk_font(ctx: &Context) -> Result<LoadedFont, String> {
    let found = discover().ok_or_else(|| {
        "未找到可用的中文字体，界面里的中文可能显示为方块。\
         请安装任一 CJK 字体（如 fonts-noto-cjk / wqy-microhei）后重启。"
            .to_string()
    })?;

    let bytes = fs::read(&found.path)
        .map_err(|e| format!("读取字体 {} 失败：{e}", found.path.display()))?;

    let mut definitions = FontDefinitions::default();
    let key = "cjk_fallback".to_owned();
    definitions.font_data.insert(
        key.clone(),
        Arc::new(FontData {
            font: Cow::Owned(bytes),
            index: found.index,
            tweak: Default::default(),
        }),
    );

    // 追加到末尾：拉丁字符仍用 egui 自带字体，缺字时回退到中文字体。
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        definitions.families.entry(family).or_default().push(key.clone());
    }

    ctx.set_fonts(definitions);
    Ok(found)
}

fn discover() -> Option<LoadedFont> {
    from_fontconfig()
        .or_else(from_known_paths)
        .or_else(from_font_dirs)
}

/// 优先问 fontconfig，它能给出最合适的字面索引。
fn from_fontconfig() -> Option<LoadedFont> {
    for pattern in ["sans-serif:lang=zh-cn", "sans-serif:lang=zh"] {
        let output = Command::new("fc-match")
            .args(["-f", "%{file}\t%{index}", pattern])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut parts = text.trim().split('\t');
        let Some(file) = parts.next().filter(|f| !f.is_empty()) else {
            continue;
        };
        let index = parts
            .next()
            .and_then(|i| i.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let path = PathBuf::from(file);
        if is_font_file(&path) {
            return Some(LoadedFont { path, index });
        }
    }
    None
}

fn from_known_paths() -> Option<LoadedFont> {
    let mut candidates: Vec<(PathBuf, u32)> = KNOWN_FONTS
        .iter()
        .map(|(p, i)| (PathBuf::from(p), *i))
        .collect();

    // 用户自己装的字体目录。
    for dir in user_font_dirs() {
        for (name, index) in [("NotoSansSC-VF.ttf", 0), ("NotoSansCJK-Regular.ttc", 2)] {
            candidates.push((dir.join(name), index));
        }
    }

    candidates
        .into_iter()
        .find(|(path, _)| is_font_file(path))
        .map(|(path, index)| LoadedFont { path, index })
}

/// 最后兜底：在常见字体目录里按文件名关键字扫描。
fn from_font_dirs() -> Option<LoadedFont> {
    let mut dirs: Vec<PathBuf> = SCAN_DIRS.iter().map(PathBuf::from).collect();
    dirs.extend(user_font_dirs());

    let mut best: Option<(usize, LoadedFont)> = None;
    let mut budget: usize = 20_000;

    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        scan(&dir, 0, &mut budget, &mut best);
        if best.as_ref().is_some_and(|(score, _)| *score == 0) {
            break;
        }
    }

    best.map(|(_, font)| font)
}

fn scan(
    dir: &Path,
    depth: usize,
    budget: &mut usize,
    best: &mut Option<(usize, LoadedFont)>,
) {
    if depth > 3 || *budget == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;

        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            scan(&path, depth + 1, budget, best);
            continue;
        }
        if !is_font_file(&path) {
            continue;
        }

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let lowered = name.to_ascii_lowercase();
        let Some(score) = NAME_KEYWORDS
            .iter()
            .position(|kw| lowered.contains(kw))
        else {
            continue;
        };

        let improved = best.as_ref().is_none_or(|(current, _)| score < *current);
        if improved {
            *best = Some((score, LoadedFont { path, index: 0 }));
        }
    }
}

fn user_font_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let home = PathBuf::from(home);
        out.push(home.join(".local").join("share").join("fonts"));
        out.push(home.join(".fonts"));
    }
    out
}

fn is_font_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(ext.to_ascii_lowercase().as_str(), "ttf" | "ttc" | "otf" | "otc")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_cjk_font_on_this_machine() {
        let font = discover().expect("测试机上应存在至少一款中文字体");
        assert!(font.path.is_file(), "{:?} 应存在", font.path);
        assert!(is_font_file(&font.path));
        assert!(!font.display_name().is_empty());
    }

    #[test]
    fn rejects_non_font_files() {
        assert!(!is_font_file(Path::new("/etc/hostname")));
        assert!(!is_font_file(Path::new("/tmp/does-not-exist.ttf")));
    }

    #[test]
    fn display_name_mentions_face_index() {
        let font = LoadedFont {
            path: PathBuf::from("/usr/share/fonts/x.ttc"),
            index: 2,
        };
        assert_eq!(font.display_name(), "x.ttc (face 2)");
    }
}
