//! Background-only preview and application of a desktop layout import.

use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE},
        System::Threading::CreateMutexW,
    },
};
use zsdesk_core::model::desktop::{
    DesktopConfig, Fence, FenceSort, FenceSource, Placement, CONFIG_VERSION, MIN_FENCE_H,
    MIN_FENCE_W, TITLE_H,
};

use super::{
    host::{self, Monitor},
    tencent_import::{self, ImportPlan, ImportedFence, PixelRect, SourceIdentity},
};

#[derive(Serialize)]
struct ImportReport {
    status: &'static str,
    source_identity: SourceIdentity,
    original_config_sha256: Option<String>,
    target_config: PathBuf,
    backup_path: Option<PathBuf>,
    source_fence_count: usize,
    mapped_fence_count: usize,
    source_member_count: usize,
    root_item_count: usize,
    result_fence_count: usize,
    result_member_count: usize,
    missing_members: Vec<String>,
    missing_mapped_folders: Vec<PathBuf>,
    warnings: Vec<String>,
    configuration: DesktopConfig,
}

/// No window, menu, tray or desktop icon state is changed by this command.
pub fn run(source: &Path, config_path: &Path, report_path: &Path, apply: bool) -> Result<()> {
    let source = if source == Path::new("-") {
        tencent_import::default_source_path().context("无法确定腾讯桌面布局路径")?
    } else {
        std::path::absolute(source)?
    };
    let config_path = std::path::absolute(config_path)?;
    let report_path = std::path::absolute(report_path)?;
    let backup_path = config_path.with_file_name("desktop-fences.before-tencent.json");
    ensure_distinct(&report_path, &[&source, &config_path, &backup_path])?;
    ensure_distinct(&source, &[&config_path, &backup_path])?;
    let plan = tencent_import::read_plan(&source)?;
    let original = read_existing(&config_path)?;
    let previous = match &original {
        Some(bytes) => serde_json::from_slice::<DesktopConfig>(bytes)
            .context("现有桌面格子配置损坏，导入已停止")?,
        None => DesktopConfig::default(),
    };
    ensure!(
        previous.version == CONFIG_VERSION,
        "现有桌面格子配置版本不受支持"
    );
    let monitors = host::monitors();
    ensure!(!monitors.is_empty(), "无法读取当前显示器布局");
    let (configuration, mut warnings) = convert(&plan, &previous, &monitors)?;
    warnings.splice(0..0, plan.warnings.iter().cloned());
    let missing_members: Vec<_> = plan
        .fences
        .iter()
        .flat_map(|f| &f.members)
        .chain(&plan.root_items)
        .filter(|i| !i.key.starts_with("::{") && !Path::new(&i.key).exists())
        .map(|i| i.key.clone())
        .collect();
    let missing_mapped_folders: Vec<_> = plan
        .fences
        .iter()
        .filter_map(|f| match &f.source {
            FenceSource::Folder { root } if !root.is_dir() => Some(root.clone()),
            _ => None,
        })
        .collect();
    let source_member_count =
        plan.fences.iter().map(|f| f.members.len()).sum::<usize>() + plan.root_items.len();
    let result_member_count = configuration
        .fences
        .iter()
        .map(|f| f.members.len())
        .sum::<usize>();
    ensure!(
        result_member_count == source_member_count,
        "布局转换发生项目数量变化，导入已停止"
    );
    let mut report = ImportReport {
        status: "preview",
        source_identity: plan.source_identity.clone(),
        original_config_sha256: original.as_ref().map(|v| digest(v)),
        target_config: config_path.clone(),
        backup_path: original.as_ref().map(|_| backup_path.clone()),
        source_fence_count: plan.fences.len(),
        mapped_fence_count: plan
            .fences
            .iter()
            .filter(|f| matches!(f.source, FenceSource::Folder { .. }))
            .count(),
        source_member_count,
        root_item_count: plan.root_items.len(),
        result_fence_count: configuration.fences.len(),
        result_member_count,
        missing_members,
        missing_mapped_folders,
        warnings,
        configuration,
    };
    write_report(&report_path, &report)?;
    if !apply {
        return Ok(());
    }
    ensure!(
        report.missing_mapped_folders.is_empty(),
        "存在无法访问的映射目录，未应用导入；详见报告"
    );
    ensure!(
        report.missing_members.is_empty(),
        "存在无法访问的项目，未应用导入；详见报告"
    );
    let _exclusive = DesktopImportLock::acquire(&config_path)?;
    verify_inputs(
        &source,
        &plan.source_identity.sha256,
        &config_path,
        original.as_deref(),
    )?;
    if let Some(bytes) = &original {
        backup_exact(&backup_path, bytes)?;
    }
    verify_inputs(
        &source,
        &plan.source_identity.sha256,
        &config_path,
        original.as_deref(),
    )?;
    host::save_config(&config_path, &report.configuration)
        .context("无法原子保存导入后的桌面布局")?;
    let saved: DesktopConfig =
        serde_json::from_slice(&fs::read(&config_path)?).context("导入后的配置回读失败")?;
    ensure!(
        saved == report.configuration,
        "导入后的配置与预览不一致，请保留备份并检查配置文件"
    );
    report.status = "applied";
    write_report(&report_path, &report)
        .context("布局已应用且回读通过，但更新应用报告失败；请核对目标配置")?;
    Ok(())
}

struct DesktopImportLock(HANDLE);
impl DesktopImportLock {
    fn acquire(config: &Path) -> Result<Self> {
        let name = super::instance_key(config);
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr()))? };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            let _ = unsafe { CloseHandle(handle) };
            bail!("桌面格子进程仍在运行；请先正常退出该配置的桌面格子后再应用导入");
        }
        Ok(Self(handle))
    }
}
impl Drop for DesktopImportLock {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_existing(path: &Path) -> Result<Option<Vec<u8>>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let metadata = path.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 16 * 1024 * 1024,
        "现有桌面配置文件大小或类型无效"
    );
    let bytes = fs::read(path)?;
    ensure!(
        bytes.len() as u64 == metadata.len(),
        "现有桌面配置正在变化，请重试"
    );
    Ok(Some(bytes))
}

fn verify_inputs(
    source: &Path,
    expected_source: &str,
    target: &Path,
    expected_target: Option<&[u8]>,
) -> Result<()> {
    ensure!(
        digest(&fs::read(source)?) == expected_source,
        "腾讯布局在预览后发生变化，导入已停止，请重新预览"
    );
    ensure!(
        read_existing(target)?.as_deref() == expected_target,
        "ZSDesk配置在预览后发生变化，导入已停止，请重新预览"
    );
    Ok(())
}

fn backup_exact(path: &Path, bytes: &[u8]) -> Result<()> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::read(path)? == bytes,
                "已有腾讯导入备份与当前配置不同，未覆盖备份：{}",
                path.display()
            );
        }
        Err(error) => return Err(error).context("无法建立桌面布局备份"),
    }
    ensure!(fs::read(path)? == bytes, "桌面布局备份校验失败");
    Ok(())
}

fn normalized_path(path: &Path) -> Result<String> {
    Ok(fs::canonicalize(path)
        .unwrap_or(std::path::absolute(path)?)
        .to_string_lossy()
        .to_lowercase())
}

fn ensure_distinct(path: &Path, protected: &[&Path]) -> Result<()> {
    let actual = normalized_path(path)?;
    for other in protected {
        ensure!(
            actual != normalized_path(other)?,
            "导入报告、源文件、目标配置和备份路径不能相同"
        );
    }
    Ok(())
}

fn write_report(path: &Path, report: &ImportReport) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("import-report.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(report)?)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn convert(
    plan: &ImportPlan,
    previous: &DesktopConfig,
    monitors: &[Monitor],
) -> Result<(DesktopConfig, Vec<String>)> {
    ensure!(!monitors.is_empty(), "当前没有可用显示器");
    let mut next = previous.clone();
    next.fences.clear();
    next.stash.clear();
    next.next_id = 1;
    next.settings.organize = true;
    next.settings.icons_hidden_by_zsdesk = false;
    let mut warnings = Vec::new();
    for imported in &plan.fences {
        let monitor = select_monitor(imported, monitors);
        let (placement, adjusted) = placement_for(imported, monitor)?;
        if adjusted {
            warnings.push(format!(
                "格子“{}”已限制在当前显示器工作区内",
                imported.title
            ));
        }
        let mut fence = Fence::new(
            next.take_id(),
            &imported.title,
            imported.source.clone(),
            placement,
        );
        fence.collapsed = imported.collapsed;
        fence.sort = FenceSort::Manual;
        fence.members = imported.members.iter().map(|i| i.key.clone()).collect();
        next.fences.push(fence);
    }
    // A separate catch-all keeps the original seven group memberships intact.
    let primary = monitors.iter().find(|m| m.primary).unwrap_or(&monitors[0]);
    let placement = free_placement(&next.fences, primary)?;
    let mut remaining = Fence::new(
        next.take_id(),
        "未分组桌面项目",
        FenceSource::Desktop,
        placement,
    );
    remaining.collapsed = true;
    remaining.catch_all = true;
    remaining.members = plan.root_items.iter().map(|i| i.key.clone()).collect();
    next.fences.push(remaining);
    // DesktopConfig::normalize must not alter the copied interaction settings.
    ensure!(
        next.fences.iter().all(|f| f.members.len() <= 5000),
        "分组项目数量超过支持上限"
    );
    let mut desktop_members = HashSet::new();
    for fence in next.fences.iter().filter(|f| f.is_desktop()) {
        for key in &fence.members {
            ensure!(
                desktop_members.insert(key.to_lowercase()),
                "同一桌面项目重复归属多个格子，未应用导入：{key}"
            );
        }
    }
    Ok((next, warnings))
}

fn select_monitor<'a>(fence: &ImportedFence, monitors: &'a [Monitor]) -> &'a Monitor {
    if let Some(screen) = &fence.screen {
        if let Some(monitor) = monitors
            .iter()
            .find(|m| same_rect(m.work, screen.work_area))
        {
            return monitor;
        }
        if screen.primary {
            if let Some(monitor) = monitors.iter().find(|m| m.primary) {
                return monitor;
            }
        }
    }
    let r = if fence.collapsed {
        fence.expanded_rect
    } else {
        fence.current_rect
    };
    let x = r.left + r.width() / 2;
    let y = r.top + r.height() / 2;
    monitors
        .iter()
        .find(|m| x >= m.work.left && x < m.work.right && y >= m.work.top && y < m.work.bottom)
        .or_else(|| monitors.iter().find(|m| m.primary))
        .unwrap_or(&monitors[0])
}

fn same_rect(a: windows::Win32::Foundation::RECT, b: PixelRect) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

fn pixels_to_dip(value: i32, dpi: u32) -> i32 {
    let divisor = i64::from(dpi.max(96));
    let numerator = i64::from(value) * 96;
    ((numerator
        + if numerator < 0 {
            -(divisor / 2)
        } else {
            divisor / 2
        })
        / divisor) as i32
}

fn placement_for(fence: &ImportedFence, monitor: &Monitor) -> Result<(Placement, bool)> {
    let work = monitor.work;
    let available_w = pixels_to_dip(work.right - work.left, monitor.dpi);
    let available_h = pixels_to_dip(work.bottom - work.top, monitor.dpi);
    ensure!(
        available_w >= MIN_FENCE_W && available_h >= MIN_FENCE_H,
        "显示器工作区太小"
    );
    let origin = fence
        .screen
        .as_ref()
        .map(|s| (s.work_area.left, s.work_area.top))
        .unwrap_or((work.left, work.top));
    let r = if fence.collapsed {
        fence.expanded_rect
    } else {
        fence.current_rect
    };
    let requested = Placement {
        monitor: monitor.name.clone(),
        x: pixels_to_dip(fence.current_rect.left - origin.0, monitor.dpi),
        y: pixels_to_dip(fence.current_rect.top - origin.1, monitor.dpi),
        width: pixels_to_dip(r.width(), monitor.dpi),
        height: pixels_to_dip(r.height(), monitor.dpi),
    };
    let width = requested.width.clamp(MIN_FENCE_W, available_w);
    let height = requested.height.clamp(MIN_FENCE_H, available_h);
    let visible_h = if fence.collapsed { TITLE_H } else { height };
    let actual = Placement {
        monitor: requested.monitor.clone(),
        width,
        height,
        x: requested.x.clamp(0, available_w - width),
        y: requested.y.clamp(0, available_h - visible_h),
    };
    let adjusted = actual != requested;
    Ok((actual, adjusted))
}

fn free_placement(fences: &[Fence], monitor: &Monitor) -> Result<Placement> {
    let max_w = pixels_to_dip(monitor.work.right - monitor.work.left, monitor.dpi);
    let max_h = pixels_to_dip(monitor.work.bottom - monitor.work.top, monitor.dpi);
    ensure!(
        max_w >= MIN_FENCE_W && max_h >= MIN_FENCE_H,
        "没有可放置未分组项目的工作区"
    );
    let width = 260.min(max_w);
    let height = 260.min(max_h);
    let occupied: Vec<_> = fences
        .iter()
        .filter(|f| f.placement.monitor == monitor.name)
        .map(|f| {
            let p = &f.placement;
            (
                p.x,
                p.y,
                p.x + p.width,
                p.y + if f.collapsed { TITLE_H } else { p.height },
            )
        })
        .collect();
    for y in (0..=max_h - TITLE_H).step_by(16) {
        for x in (0..=max_w - width).step_by(16) {
            let overlaps = occupied.iter().any(|&(l, t, r, b)| {
                x < r + 8 && x + width + 8 > l && y < b + 8 && y + TITLE_H + 8 > t
            });
            if !overlaps {
                return Ok(Placement {
                    monitor: monitor.name.clone(),
                    x,
                    y,
                    width,
                    height,
                });
            }
        }
    }
    bail!("当前屏幕没有可放置未分组桌面项目的空位，导入已停止")
}

#[cfg(test)]
mod tests {
    use super::super::tencent_import::{ImportedItem, ImportedScreen};
    use super::*;
    use windows::Win32::Foundation::RECT;
    use zsdesk_core::model::desktop::FenceTheme;

    fn monitor() -> Monitor {
        Monitor {
            name: "display".into(),
            work: RECT {
                left: 1920,
                top: 0,
                right: 3840,
                bottom: 1040,
            },
            dpi: 144,
            primary: true,
        }
    }
    fn imported() -> ImportedFence {
        ImportedFence {
            source_id: 11,
            title: "资料".into(),
            source: FenceSource::Folder {
                root: "D:/docs".into(),
            },
            current_rect: PixelRect {
                left: 2070,
                top: 30,
                right: 2520,
                bottom: 68,
            },
            expanded_rect: PixelRect {
                left: 2070,
                top: 30,
                right: 2520,
                bottom: 630,
            },
            collapsed: true,
            screen: Some(ImportedScreen {
                device_id: "source".into(),
                bounds: PixelRect {
                    left: 1920,
                    top: 0,
                    right: 3840,
                    bottom: 1080,
                },
                work_area: PixelRect {
                    left: 1920,
                    top: 0,
                    right: 3840,
                    bottom: 1040,
                },
                primary: true,
            }),
            members: vec![ImportedItem {
                name: "a".into(),
                key: "D:/docs/a".into(),
                order: 0,
                source_item_id: 5,
                kind: 4,
            }],
        }
    }
    #[test]
    fn source_pixels_are_converted_once_relative_to_work_area() {
        let (placement, adjusted) = placement_for(&imported(), &monitor()).unwrap();
        assert_eq!(
            (placement.x, placement.y, placement.width, placement.height),
            (100, 20, 300, 400)
        );
        assert!(!adjusted);
        assert_eq!(pixels_to_dip(-150, 144), -100);
    }
    #[test]
    fn expanded_window_uses_current_geometry_instead_of_stale_restore_rect() {
        let mut fence = imported();
        fence.collapsed = false;
        fence.current_rect = PixelRect {
            left: 2100,
            top: 60,
            right: 2700,
            bottom: 510,
        };
        let (placement, adjusted) = placement_for(&fence, &monitor()).unwrap();
        assert_eq!(
            (placement.x, placement.y, placement.width, placement.height),
            (120, 40, 400, 300)
        );
        assert!(!adjusted);
    }
    #[test]
    fn conversion_preserves_interactions_and_keeps_root_members_separate() {
        let mut previous = DesktopConfig::default();
        previous.settings.theme = FenceTheme::Light;
        previous.settings.opacity = 73;
        previous.settings.hover_expand = false;
        previous.settings.double_click_toggle = false;
        previous.settings.browse_inside = false;
        previous.settings.hide_tray = true;
        previous.settings.hidden = true;
        let plan = ImportPlan {
            source_identity: SourceIdentity {
                path: "source".into(),
                byte_len: 1,
                modified_unix_ms: None,
                sha256: String::new(),
            },
            fences: vec![imported()],
            root_items: vec![ImportedItem {
                name: "root".into(),
                key: "C:/Desktop/root".into(),
                order: 0,
                source_item_id: 1,
                kind: 4,
            }],
            warnings: Vec::new(),
        };
        let (next, _) = convert(&plan, &previous, &[monitor()]).unwrap();
        let mut expected = previous.settings;
        expected.organize = true;
        expected.icons_hidden_by_zsdesk = false;
        assert_eq!(next.settings, expected);
        assert_eq!(next.fences.len(), 2);
        assert_eq!(next.fences[0].members, ["D:/docs/a"]);
        assert_eq!(next.fences[1].members, ["C:/Desktop/root"]);
        assert!(next.fences[1].collapsed && next.fences[1].catch_all);
        assert!(next.stash.is_empty());
    }
}
