//! Read-only import of DeskGo's persisted desktop layout.
//!
//! The format stores UTF-8 XML behind a deterministic RC4 codec. It is a
//! compatibility codec, not a cipher used to protect ZSDesk data. Pixel
//! rectangles remain in the source coordinate system; the desktop host owns
//! monitor selection, DPI conversion, backup and application of the plan.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use anyhow::{ensure, Context, Result};
use roxmltree::{Document, Node, ParsingOptions};
use serde::Serialize;
use sha2::{Digest, Sha256};
use zsdesk_core::model::desktop::FenceSource;

const MAX_SOURCE_BYTES: usize = 10 * 1024 * 1024;
const MAX_XML_NODES: u32 = 150_000;
const MAX_FENCES: usize = 512;
const MAX_ITEMS: usize = 25_000;
const MAX_ITEMS_PER_FENCE: usize = 5_000;
const CODEC_KEY: &[u8] = b"{8CEFC9E6-A2B4-4c2a-823C-6903A31139FA}";

#[derive(Clone, Debug, Serialize)]
pub struct SourceIdentity {
    pub path: PathBuf,
    pub byte_len: u64,
    pub modified_unix_ms: Option<u64>,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PixelRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl PixelRect {
    pub fn width(self) -> i32 {
        self.right - self.left
    }
    pub fn height(self) -> i32 {
        self.bottom - self.top
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportedScreen {
    /// DeskGo's display identity; it is not a Win32 device name.
    pub device_id: String,
    pub bounds: PixelRect,
    pub work_area: PixelRect,
    pub primary: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportedItem {
    pub name: String,
    /// An absolute file path or a Shell namespace parsing name.
    pub key: String,
    /// Sequence within the serialized group; not DeskGo's numeric item ID.
    pub order: i32,
    pub source_item_id: i32,
    pub kind: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportedFence {
    pub source_id: u64,
    pub title: String,
    pub source: FenceSource,
    pub current_rect: PixelRect,
    pub expanded_rect: PixelRect,
    pub collapsed: bool,
    pub screen: Option<ImportedScreen>,
    pub members: Vec<ImportedItem>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportPlan {
    pub source_identity: SourceIdentity,
    pub fences: Vec<ImportedFence>,
    /// Items left on the source desktop, including any orphaned group records.
    /// The host must give these a visible destination before hiding desktop icons.
    pub root_items: Vec<ImportedItem>,
    pub warnings: Vec<String>,
}

pub fn default_source_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("Tencent").join("DeskGo").join("FencesDataFile.dat"))
}

/// Reads a stable snapshot. No source or destination file is changed.
pub fn read_plan(path: &Path) -> Result<ImportPlan> {
    let (bytes, modified_unix_ms) = read_snapshot(path)?;
    let (check, check_modified) = read_snapshot(path)?;
    ensure!(
        bytes == check && modified_unix_ms == check_modified,
        "腾讯桌面布局正在保存，请稍后重试"
    );
    let identity = SourceIdentity {
        path: path.to_path_buf(),
        byte_len: bytes.len() as u64,
        modified_unix_ms,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    parse_encrypted(&bytes, identity)
}

fn read_snapshot(path: &Path) -> Result<(Vec<u8>, Option<u64>)> {
    let mut file =
        File::open(path).with_context(|| format!("无法读取腾讯桌面布局：{}", path.display()))?;
    let before = file.metadata()?;
    ensure!(before.is_file(), "布局路径不是普通文件");
    ensure!(
        before.len() > 0 && before.len() <= MAX_SOURCE_BYTES as u64,
        "腾讯桌面布局文件大小无效"
    );
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.by_ref()
        .take((MAX_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        bytes.len() <= MAX_SOURCE_BYTES
            && bytes.len() as u64 == before.len()
            && before.len() == after.len()
            && before.modified().ok() == after.modified().ok(),
        "腾讯桌面布局正在保存，请稍后重试"
    );
    let modified = after
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok());
    Ok((bytes, modified))
}

fn parse_encrypted(bytes: &[u8], identity: SourceIdentity) -> Result<ImportPlan> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_SOURCE_BYTES,
        "腾讯桌面布局文件大小无效"
    );
    let decoded = rc4(bytes, CODEC_KEY);
    let end = decoded.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let xml =
        std::str::from_utf8(&decoded[..end]).context("腾讯桌面布局格式无法识别或文件已损坏")?;
    parse_xml(xml, identity)
}

fn parse_xml(xml: &str, identity: SourceIdentity) -> Result<ImportPlan> {
    ensure!(
        !xml.is_empty() && xml.len() <= MAX_SOURCE_BYTES,
        "腾讯桌面布局内容为空或过大"
    );
    // Reject even an empty DTD. The reader never resolves external resources.
    ensure!(!xml.contains("<!DOCTYPE"), "桌面布局不支持 DTD");
    let document = Document::parse_with_options(
        xml,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: MAX_XML_NODES,
            entity_resolver: None,
        },
    )
    .context("腾讯桌面布局 XML 无效")?;
    let root = document.root_element();
    ensure!(
        root.tag_name().name() == "root" && root.tag_name().namespace().is_none(),
        "腾讯桌面布局根节点无法识别"
    );
    let boxes = section(root, "BoxList", true)?.context("腾讯桌面布局缺少格子列表")?;
    let items = section(root, "ItemList", true)?.context("腾讯桌面布局缺少项目列表")?;
    let screen_list = section(root, "BoxListScreenInfo", false)?;
    let mut plan = ImportPlan {
        source_identity: identity,
        fences: Vec::new(),
        root_items: Vec::new(),
        warnings: Vec::new(),
    };
    let mut ids = HashSet::new();
    let mut root_ids = HashSet::new();
    for record in boxes.children().filter(Node::is_element) {
        ensure!(
            matches!(record.tag_name().name(), "Box" | "Box_2"),
            "腾讯格子记录类型无法识别：{}",
            record.tag_name().name()
        );
        let text = descriptor(record, "BoxDesc")?;
        let fields: Vec<_> = text.split("<|>").collect();
        ensure!(fields.len() == 24, "腾讯格子记录字段数量不受支持");
        let id: u64 = number(fields[0], "格子编号")?;
        ensure!(id > 0 && ids.insert(id), "腾讯格子编号为空或重复：{id}");
        let title = unescape_field(fields[1]);
        let folder = unescape_field(fields[23]);
        // The unnamed ID-1 record describes the desktop, not a visible fence.
        if id == 1 && title.is_empty() && folder.is_empty() {
            root_ids.insert(id);
            continue;
        }
        ensure!(plan.fences.len() < MAX_FENCES, "腾讯格子数量超过支持上限");
        let source = if folder.is_empty() {
            ensure!(
                record.tag_name().name() != "Box_2",
                "文件夹映射格子缺少目录：{title}"
            );
            FenceSource::Desktop
        } else {
            ensure!(is_absolute_file_key(&folder), "格子“{title}”的映射目录无效");
            FenceSource::Folder {
                root: PathBuf::from(folder),
            }
        };
        let current_rect = rectangle(&fields[2..6], "格子当前位置")?;
        let expanded_rect = rectangle(&fields[19..23], "格子展开位置")?;
        let collapsed = is_title_only(current_rect, expanded_rect);
        if !collapsed && current_rect != expanded_rect {
            plan.warnings.push(format!(
                "格子“{title}”的当前位置与展开记录不同，保留当前位置"
            ));
        }
        plan.fences.push(ImportedFence {
            source_id: id,
            title,
            source,
            current_rect,
            expanded_rect,
            collapsed,
            screen: None,
            members: Vec::new(),
        });
    }
    ensure!(!plan.fences.is_empty(), "腾讯桌面布局中没有可导入的格子");
    let by_id: HashMap<_, _> = plan
        .fences
        .iter()
        .enumerate()
        .map(|(i, f)| (f.source_id, i))
        .collect();
    if let Some(screen_list) = screen_list {
        let mut screens = HashSet::new();
        for record in screen_list.children().filter(Node::is_element) {
            ensure!(
                record.tag_name().name() == "Box",
                "屏幕布局记录类型无法识别"
            );
            let text = descriptor(record, "BoxDesc")?;
            let fields: Vec<_> = text.split("<|>").collect();
            ensure!(fields.len() == 17, "腾讯屏幕记录字段数量不受支持");
            let id: u64 = number(fields[0], "屏幕格子编号")?;
            ensure!(screens.insert(id), "腾讯屏幕记录重复：{id}");
            if root_ids.contains(&id) {
                continue;
            }
            let Some(&index) = by_id.get(&id) else {
                plan.warnings
                    .push(format!("屏幕记录对应的格子 {id} 不存在"));
                continue;
            };
            let device_id = unescape_field(fields[1]);
            ensure!(!device_id.is_empty(), "格子屏幕标识为空");
            let bounds = rectangle(&fields[2..6], "显示器范围")?;
            let work_area = rectangle(&fields[6..10], "显示器工作区")?;
            ensure!(
                work_area.left >= bounds.left
                    && work_area.top >= bounds.top
                    && work_area.right <= bounds.right
                    && work_area.bottom <= bounds.bottom,
                "显示器工作区超出屏幕范围"
            );
            let primary: u32 = number(fields[10], "主显示器标记")?;
            ensure!(primary <= 1, "主显示器标记无效");
            plan.fences[index].screen = Some(ImportedScreen {
                device_id,
                bounds,
                work_area,
                primary: primary == 1,
            });
        }
    }
    let mut item_count = 0;
    let mut member_keys: HashMap<u64, HashSet<String>> = HashMap::new();
    for record in items.children().filter(Node::is_element) {
        ensure!(
            matches!(record.tag_name().name(), "Item" | "Item_2"),
            "腾讯项目记录类型无法识别"
        );
        item_count += 1;
        ensure!(item_count <= MAX_ITEMS, "腾讯桌面项目数量超过支持上限");
        let text = descriptor(record, "ItemDesc")?;
        let fields: Vec<_> = text.split("<|>").collect();
        ensure!(fields.len() == 6, "腾讯项目记录字段数量不受支持");
        let owner: u64 = number(fields[1], "项目所属格子")?;
        let key = unescape_field(fields[4]);
        ensure!(
            is_absolute_file_key(&key) || is_shell_key(&key),
            "腾讯项目路径无效：{}",
            fields[0]
        );
        let name = unescape_field(fields[0]);
        let source_item_id = number(fields[2], "源项目编号")?;
        let kind = number(fields[3], "项目类型")?;
        let _reserved: u32 = number(fields[5], "项目标记")?;
        let owner_key = if by_id.contains_key(&owner) { owner } else { 0 };
        if !member_keys
            .entry(owner_key)
            .or_default()
            .insert(key.to_lowercase())
        {
            plan.warnings
                .push(format!("项目“{name}”在同一分组中重复，保留第一次记录"));
            continue;
        }
        let destination = if let Some(&index) = by_id.get(&owner) {
            &mut plan.fences[index].members
        } else {
            if !root_ids.contains(&owner) {
                plan.warnings.push(format!(
                    "项目“{name}”所属格子 {owner} 不存在，保留为未分组项目"
                ));
            }
            &mut plan.root_items
        };
        ensure!(
            destination.len() < MAX_ITEMS_PER_FENCE,
            "单个格子的项目数量超过支持上限"
        );
        destination.push(ImportedItem {
            name,
            key,
            order: destination.len() as i32,
            source_item_id,
            kind,
        });
    }
    for fence in &plan.fences {
        if fence.screen.is_none() {
            plan.warnings.push(format!(
                "格子“{}”未保存显示器信息，将由当前屏幕布局定位",
                fence.title
            ));
        }
    }
    Ok(plan)
}

fn section<'a, 'input>(
    parent: Node<'a, 'input>,
    name: &str,
    required: bool,
) -> Result<Option<Node<'a, 'input>>> {
    let mut matches = parent
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == name);
    let first = matches.next();
    ensure!(matches.next().is_none(), "腾讯布局节点重复：{name}");
    ensure!(!required || first.is_some(), "腾讯布局缺少节点：{name}");
    Ok(first)
}

fn descriptor(record: Node<'_, '_>, name: &str) -> Result<String> {
    let node = section(record, name, true)?.context("腾讯布局记录为空")?;
    ensure!(
        !node.children().any(|n| n.is_element()),
        "腾讯布局描述节点包含异常子节点"
    );
    let text: String = node.children().filter_map(|n| n.text()).collect();
    let text = text.trim();
    ensure!(
        !text.is_empty() && text.len() <= 64 * 1024,
        "腾讯布局记录为空或过大"
    );
    Ok(text.to_string())
}

fn number<T: std::str::FromStr>(text: &str, label: &str) -> Result<T> {
    text.parse()
        .map_err(|_| anyhow::anyhow!("腾讯布局的{label}无效"))
}

fn rectangle(fields: &[&str], label: &str) -> Result<PixelRect> {
    ensure!(fields.len() == 4, "{label}字段数量无效");
    let rect = PixelRect {
        left: number(fields[0], label)?,
        top: number(fields[1], label)?,
        right: number(fields[2], label)?,
        bottom: number(fields[3], label)?,
    };
    let width = i64::from(rect.right) - i64::from(rect.left);
    let height = i64::from(rect.bottom) - i64::from(rect.top);
    ensure!(
        width > 0
            && height > 0
            && width <= 1_000_000
            && height <= 1_000_000
            && [rect.left, rect.top, rect.right, rect.bottom]
                .iter()
                .all(|v| i64::from(*v).abs() <= 1_000_000),
        "{label}不是有效矩形"
    );
    Ok(rect)
}

fn is_title_only(current: PixelRect, expanded: PixelRect) -> bool {
    current.left == expanded.left
        && current.top == expanded.top
        && current.right == expanded.right
        && (16..=64).contains(&current.height())
        && current.height() < expanded.height()
}

fn unescape_field(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek() == Some(&'\\') {
            chars.next();
        }
        result.push(ch);
    }
    result
}

fn is_absolute_file_key(key: &str) -> bool {
    if key.contains('\0') || key.chars().any(char::is_control) {
        return false;
    }
    let bytes = key.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
    {
        return true;
    }
    if let Some(rest) = key.strip_prefix("\\\\") {
        let mut parts = rest.split('\\');
        return parts.next().is_some_and(|p| !p.is_empty())
            && parts.next().is_some_and(|p| !p.is_empty());
    }
    false
}

fn is_shell_key(key: &str) -> bool {
    if key.contains('\0') || key.chars().any(char::is_control) {
        return false;
    }
    key.starts_with("::{")
        && key.get(2..40).is_some_and(|id| {
            let bytes = id.as_bytes();
            bytes.len() == 38
                && bytes[0] == b'{'
                && bytes[37] == b'}'
                && bytes[1..37].iter().enumerate().all(|(i, b)| {
                    if matches!(i, 8 | 13 | 18 | 23) {
                        *b == b'-'
                    } else {
                        b.is_ascii_hexdigit()
                    }
                })
        })
}

fn rc4(input: &[u8], key: &[u8]) -> Vec<u8> {
    let mut state = [0u8; 256];
    for (i, value) in state.iter_mut().enumerate() {
        *value = i as u8;
    }
    let mut j = 0usize;
    for i in 0..256 {
        j = (j + usize::from(state[i]) + usize::from(key[i % key.len()])) & 255;
        state.swap(i, j);
    }
    let (mut i, mut j) = (0usize, 0usize);
    input
        .iter()
        .map(|byte| {
            i = (i + 1) & 255;
            j = (j + usize::from(state[i])) & 255;
            state.swap(i, j);
            byte ^ state[(usize::from(state[i]) + usize::from(state[j])) & 255]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> SourceIdentity {
        SourceIdentity {
            path: "layout.dat".into(),
            byte_len: 0,
            modified_unix_ms: None,
            sha256: String::new(),
        }
    }

    fn box_record(id: u64, title: &str, path: &str) -> String {
        format!("<Box><BoxDesc><![CDATA[{id}<|>{title}<|>100<|>10<|>340<|>48<|>0<|>0<|>1<|>1<|>0<|>0<|>0<|>0<|>0<|>0<|>0<|>0<|>0<|>100<|>10<|>340<|>410<|>{path}]]></BoxDesc></Box>")
    }

    fn layout(boxes: &str, items: &str) -> String {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><root><BoxList>{boxes}</BoxList><ItemList>{items}</ItemList></root>")
    }

    fn item(name: &str, owner: u64, id: i32, key: &str) -> String {
        format!(
            "<Item><ItemDesc><![CDATA[{name}<|>{owner}<|>{id}<|>4<|>{key}<|>0]]></ItemDesc></Item>"
        )
    }

    #[test]
    fn rc4_matches_known_vector_and_layout_prefix() {
        assert_eq!(
            rc4(b"Plaintext", b"Key"),
            [0xbb, 0xf3, 0x16, 0xe8, 0xd9, 0x40, 0xaf, 0x0a, 0xd3]
        );
        let captured = [
            0x47, 0x4b, 0x92, 0x5a, 0xf7, 0x32, 0xc3, 0x02, 0xe7, 0x76, 0x87, 0xf1, 0x96, 0xc2,
            0x96, 0x47,
        ];
        assert_eq!(rc4(&captured, CODEC_KEY), b"<?xml version=\"1");
    }

    #[test]
    fn escaped_paths_preserve_unc_and_literal_letters() {
        assert_eq!(
            unescape_field(r"C:\\new\\test\\资料.txt"),
            r"C:\new\test\资料.txt"
        );
        assert_eq!(
            unescape_field(r"\\\\server\\share\\new"),
            r"\\server\share\new"
        );
        assert!(is_absolute_file_key(r"\\server\share\new"));
        assert!(!is_absolute_file_key(r"C:relative"));
        assert!(!is_absolute_file_key("../relative"));
        assert!(is_shell_key("::{645FF040-5081-101B-9F08-00AA002F954E}"));
        assert!(!is_shell_key("::{invalid}"));
    }

    #[test]
    fn importer_preserves_groups_root_items_and_serialized_order() {
        let root = "<Box><BoxDesc><![CDATA[1<|><|>0<|>0<|>1920<|>1040<|>0<|>0<|>3<|>4<|>1<|>0<|>0<|>0<|>0<|>0<|>0<|>0<|>1<|>0<|>0<|>0<|>0<|>]]></BoxDesc></Box>";
        let boxes = format!(
            "{root}{}{}",
            box_record(11, "文档", ""),
            box_record(12, "映射", r"D:\\资料")
        );
        let records = format!(
            "{}{}{}{}{}",
            item("根项目", 1, 1, r"C:\\Desktop\\root.txt"),
            item("先保存", 11, 101, r"C:\\Desktop\\first.txt"),
            item("后保存", 11, 1, r"C:\\Desktop\\second.txt"),
            item("目录项", 12, 0, r"D:\\资料\\file.txt"),
            item("孤立项目", 99, 7, r"C:\\Desktop\\orphan.txt")
        );
        let plan = parse_xml(&layout(&boxes, &records), identity()).unwrap();
        assert_eq!(plan.fences.len(), 2);
        assert_eq!(plan.root_items.len(), 2);
        assert_eq!(
            plan.fences[0]
                .members
                .iter()
                .map(|i| (&*i.name, i.order, i.source_item_id))
                .collect::<Vec<_>>(),
            [("先保存", 0, 101), ("后保存", 1, 1)]
        );
        assert_eq!(
            plan.fences[1].source,
            FenceSource::Folder {
                root: PathBuf::from(r"D:\资料")
            }
        );
        assert_eq!(plan.fences[1].members[0].key, r"D:\资料\file.txt");
        assert!(plan.warnings.iter().any(|s| s.contains("孤立项目")));
        assert!(plan.fences[0].collapsed);
    }

    #[test]
    fn collapsed_requires_a_valid_title_only_geometry() {
        let expanded = PixelRect {
            left: 10,
            top: 10,
            right: 250,
            bottom: 410,
        };
        let title = PixelRect {
            bottom: 48,
            ..expanded
        };
        assert!(is_title_only(title, expanded));
        assert!(!is_title_only(
            PixelRect {
                bottom: 200,
                ..title
            },
            expanded
        ));
        assert!(!is_title_only(PixelRect { left: 20, ..title }, expanded));
        assert!(!is_title_only(expanded, expanded));
        assert!(rectangle(&["10", "10", "5", "20"], "test").is_err());
        assert!(rectangle(&["-2147483648", "0", "2147483647", "20"], "test").is_err());
    }

    #[test]
    fn corrupt_or_unsupported_layouts_never_succeed_as_empty_imports() {
        assert!(parse_encrypted(b"broken encrypted data", identity()).is_err());
        assert!(parse_xml("<root><BoxList/><ItemList/></root>", identity()).is_err());
        assert!(parse_xml("<wrong/>", identity()).is_err());
        let valid = layout(&box_record(11, "资料", ""), "");
        let duplicate = valid.replace("</root>", "<ItemList/></root>");
        assert!(parse_xml(&duplicate, identity()).is_err());
        let dtd = valid.replace(
            "<root>",
            "<!DOCTYPE root [<!ENTITY x SYSTEM 'file:///private'>]><root>",
        );
        assert!(parse_xml(&dtd, identity()).is_err());
        let unknown = valid
            .replace("<Box>", "<Unknown>")
            .replace("</Box>", "</Unknown>");
        assert!(parse_xml(&unknown, identity()).is_err());
    }

    #[test]
    fn encrypted_xml_roundtrip_accepts_trailing_nul_and_preserves_virtual_items() {
        let xml = layout(
            &box_record(11, "桌面", ""),
            &item(
                "回收站",
                11,
                101,
                "::{645FF040-5081-101B-9F08-00AA002F954E}",
            ),
        );
        let mut plain = xml.into_bytes();
        plain.push(0);
        let plan = parse_encrypted(&rc4(&plain, CODEC_KEY), identity()).unwrap();
        assert_eq!(
            plan.fences[0].members[0].key,
            "::{645FF040-5081-101B-9F08-00AA002F954E}"
        );
        let mut invalid = plain;
        invalid[0] = 0xff;
        assert!(parse_encrypted(&rc4(&invalid, CODEC_KEY), identity()).is_err());
    }

    #[test]
    fn screen_work_area_is_checked_and_kept_in_source_pixels() {
        let screen = "<BoxListScreenInfo><Box><BoxDesc><![CDATA[11<|>PrimaryDisplay:test<|>0<|>0<|>2560<|>1440<|>0<|>0<|>2560<|>1392<|>1<|>-1<|>-1<|>-1<|>-1<|>-1<|>-1]]></BoxDesc></Box></BoxListScreenInfo>";
        let xml =
            layout(&box_record(11, "资料", ""), "").replace("</root>", &format!("{screen}</root>"));
        let plan = parse_xml(&xml, identity()).unwrap();
        let screen = plan.fences[0].screen.as_ref().unwrap();
        assert_eq!(screen.work_area.height(), 1392);
        assert!(screen.primary);
        assert!(parse_xml(&xml.replace("2560<|>1392", "2560<|>2000"), identity()).is_err());
    }
}
