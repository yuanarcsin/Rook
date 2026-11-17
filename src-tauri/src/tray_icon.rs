//! 托盘图标聚合角标（纯逻辑，不依赖 Tauri）。
//!
//! 输入是基础图标的 RGBA 像素（行优先、自顶向下）与宽高，输出是一份**新**像素：
//! 右下角多了一个实心圆点，颜色对应聚合级别（红 > 琥珀 > 绿 > 灰，见
//! [`crate::tray_summary::AggregateLevel`]）。基础图标本身不被修改，角标以外的
//! 像素逐字节保留。
//!
//! 之所以独立成模块：`tray` 是 `cfg(not(test))`（测试二进制不链接 WebView2/托盘），
//! 像素绘制放在这里才能被 `cargo test` 直接覆盖。
//!
//! # 几何与配色
//!
//! - 直径：`宽度 × 40%` 四舍五入，夹到 `[1, min(宽, 高)]`，因此永远画得下、不越界；
//!   非正方形图标（如 64×16）以高度为上限，避免角标被裁成椭圆。
//! - 位置：角标外接框右下对齐（`x0 = 宽 - 直径`、`y0 = 高 - 直径`），圆心落在
//!   外接框中心，超出圆形之外的角落像素保持原样。
//! - 描边：直径 ≥ 4 时画 1px 描边环。描边颜色按角标区域的平均亮度选：浅色背景用
//!   深色 `#111827`，深色/透明背景用白色 `#FFFFFF`，保证深浅背景下都能看出边界。
//! - 填充：红 `#DC2626`、琥珀 `#D97706`、绿 `#16A34A`、灰 `#6B7280`，全部不透明。

use crate::tray_summary::AggregateLevel;

/// 角标直径相对图标宽度的比例（约 40%）。
pub(crate) const BADGE_RATIO: f64 = 0.40;

/// 直径小于该值时不画描边，否则描边会把整个角标吃光（小尺寸图标）。
const MIN_STROKED_DIAMETER: u32 = 4;

/// 描边环宽度（像素）。
const STROKE_WIDTH: f64 = 1.0;

/// 红：暂停 / 需要人工 / 配置无效 / 整份配置错误。
pub(crate) const RED: [u8; 4] = [0xDC, 0x26, 0x26, 0xFF];
/// 琥珀：退避重试、启动失败、启动中/停止中。
pub(crate) const AMBER: [u8; 4] = [0xD9, 0x77, 0x06, 0xFF];
/// 绿：至少一个运行中且无异常。
pub(crate) const GREEN: [u8; 4] = [0x16, 0xA3, 0x4A, 0xFF];
/// 灰：没有任务，或全部已停止/已禁用。
pub(crate) const GRAY: [u8; 4] = [0x6B, 0x72, 0x80, 0xFF];
/// 深色描边（浅色背景）。
pub(crate) const DARK_STROKE: [u8; 4] = [0x11, 0x18, 0x27, 0xFF];
/// 白色描边（深色/透明背景）。
pub(crate) const LIGHT_STROKE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

/// 聚合级别 → 角标填充色。
pub(crate) fn level_color(level: AggregateLevel) -> [u8; 4] {
    match level {
        AggregateLevel::Red => RED,
        AggregateLevel::Amber => AMBER,
        AggregateLevel::Green => GREEN,
        AggregateLevel::Gray => GRAY,
    }
}

/// 角标直径：宽度 40% 四舍五入，夹到 `[1, min(宽, 高)]`；任一维为 0 时返回 0。
pub(crate) fn badge_diameter(width: u32, height: u32) -> u32 {
    if width == 0 || height == 0 {
        return 0;
    }
    let scaled = (f64::from(width) * BADGE_RATIO).round() as u32;
    scaled.clamp(1, width.min(height))
}

/// 在基础图标右下角绘制聚合角标，返回新像素；尺寸/长度不合法时返回 `None`。
///
/// 调用方（`tray`）在 `None` 时保留原图标并记日志，不做兜底绘制。
pub(crate) fn draw_badge(
    rgba: &[u8],
    width: u32,
    height: u32,
    level: AggregateLevel,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if rgba.len() != expected {
        return None;
    }

    let diameter = badge_diameter(width, height);
    let x0 = width - diameter;
    let y0 = height - diameter;
    let radius = f64::from(diameter) / 2.0;
    let center_x = f64::from(x0) + radius;
    let center_y = f64::from(y0) + radius;
    let fill = level_color(level);
    let stroke = if diameter >= MIN_STROKED_DIAMETER {
        Some((
            pick_stroke(rgba, width, x0, y0, diameter),
            radius - STROKE_WIDTH,
        ))
    } else {
        None
    };

    let mut pixels = rgba.to_vec();
    for y in y0..(y0 + diameter) {
        for x in x0..(x0 + diameter) {
            let dx = f64::from(x) + 0.5 - center_x;
            let dy = f64::from(y) + 0.5 - center_y;
            if (dx * dx + dy * dy).sqrt() > radius {
                // 圆外的角落：保留基础图标像素。
                continue;
            }
            let color = match stroke {
                Some((stroke, inner)) if (dx * dx + dy * dy).sqrt() >= inner => stroke,
                _ => fill,
            };
            let offset = ((y as usize) * (width as usize) + (x as usize)) * 4;
            pixels[offset..offset + 4].copy_from_slice(&color);
        }
    }
    Some(pixels)
}

/// 按角标区域的平均亮度选描边色：浅背景用深色，深背景/透明用白色。
///
/// 透明像素按黑底计入（乘 alpha），因此全透明图标会拿到白色描边。
fn pick_stroke(rgba: &[u8], width: u32, x0: u32, y0: u32, diameter: u32) -> [u8; 4] {
    let mut total = 0.0_f64;
    let mut count = 0.0_f64;
    for y in y0..(y0 + diameter) {
        for x in x0..(x0 + diameter) {
            let offset = ((y as usize) * (width as usize) + (x as usize)) * 4;
            let alpha = f64::from(rgba[offset + 3]) / 255.0;
            let luma = 0.299 * f64::from(rgba[offset])
                + 0.587 * f64::from(rgba[offset + 1])
                + 0.114 * f64::from(rgba[offset + 2]);
            total += luma * alpha;
            count += 1.0;
        }
    }
    if count > 0.0 && total / count >= 128.0 {
        DARK_STROKE
    } else {
        LIGHT_STROKE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let offset = ((y as usize) * (width as usize) + (x as usize)) * 4;
        pixels[offset..offset + 4].try_into().expect("4 字节像素")
    }

    fn transparent(width: u32, height: u32) -> Vec<u8> {
        vec![0u8; (width as usize) * (height as usize) * 4]
    }

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for _ in 0..(width as usize) * (height as usize) {
            pixels.extend_from_slice(&color);
        }
        pixels
    }

    /// 32×32 的角标：直径 13、外接框从 (19,19) 开始、圆心 (25.5,25.5)。
    #[test]
    fn each_level_paints_its_own_color() {
        let cases = [
            (AggregateLevel::Red, RED),
            (AggregateLevel::Amber, AMBER),
            (AggregateLevel::Green, GREEN),
            (AggregateLevel::Gray, GRAY),
        ];
        for (level, expected) in cases {
            let base = transparent(32, 32);
            let out = draw_badge(&base, 32, 32, level).expect("32×32 应当能绘制");
            assert_eq!(pixel(&out, 32, 25, 25), expected, "{level:?} 圆心像素");
            assert_eq!(pixel(&out, 32, 25, 25)[3], 255, "{level:?} 必须不透明");
        }
    }

    /// 四种级别颜色互不相同，且与描边色不同（避免"画了但看不出来"）。
    #[test]
    fn colors_are_distinct() {
        let fills = [RED, AMBER, GREEN, GRAY];
        for (index, left) in fills.iter().enumerate() {
            for right in &fills[index + 1..] {
                assert_ne!(left, right);
            }
            assert_ne!(left, &DARK_STROKE);
            assert_ne!(left, &LIGHT_STROKE);
        }
    }

    /// 角标只落在右下角的外接框内，框外像素一个都不动。
    #[test]
    fn badge_stays_in_bottom_right_box() {
        let width = 32;
        let base = solid(width, width, [0, 0, 0, 255]);
        let out = draw_badge(&base, width, width, AggregateLevel::Red).expect("应当能绘制");
        let diameter = badge_diameter(width, width);
        let (x0, y0) = (width - diameter, width - diameter);

        let mut painted = 0;
        for y in 0..width {
            for x in 0..width {
                if x < x0 || y < y0 {
                    assert_eq!(
                        pixel(&out, width, x, y),
                        [0, 0, 0, 255],
                        "外接框外的 ({x},{y}) 被改写了"
                    );
                } else if pixel(&out, width, x, y) != [0, 0, 0, 255] {
                    painted += 1;
                }
            }
        }
        assert!(painted > 0, "右下角应当被绘制");
        // 圆心位于右下象限，说明角标确实贴在右下而不是别处。
        assert!(x0 + diameter / 2 > width / 2);
        assert!(y0 + diameter / 2 > width / 2);
    }

    /// 圆外（外接框内但不在圆盘里）的像素保持原样：这里是 (31,31)。
    #[test]
    fn pixels_outside_the_disc_are_preserved() {
        let width = 32;
        let base = solid(width, width, [10, 20, 30, 255]);
        let out = draw_badge(&base, width, width, AggregateLevel::Green).expect("应当能绘制");
        assert_eq!(pixel(&out, width, 31, 31), [10, 20, 30, 255]);
        assert_eq!(pixel(&out, width, 0, 0), [10, 20, 30, 255]);
        assert_eq!(pixel(&out, width, 19, 31), [10, 20, 30, 255]);
    }

    /// 原始像素逐字节保留（渐变背景，角标以外一律不变）。
    #[test]
    fn original_pixels_are_preserved() {
        let width = 32;
        let mut base = Vec::with_capacity((width * width * 4) as usize);
        for y in 0..width {
            for x in 0..width {
                base.extend_from_slice(&[(x * 8) as u8, (y * 8) as u8, 128, 255]);
            }
        }
        let out = draw_badge(&base, width, width, AggregateLevel::Amber).expect("应当能绘制");
        let diameter = badge_diameter(width, width);
        let (x0, y0) = (width - diameter, width - diameter);
        for y in 0..width {
            for x in 0..width {
                if x < x0 || y < y0 {
                    assert_eq!(
                        pixel(&out, width, x, y),
                        pixel(&base, width, x, y),
                        "({x},{y}) 应当保留原像素"
                    );
                }
            }
        }
    }

    /// 全透明底图：角标不透明，其余仍是全透明。
    #[test]
    fn transparent_base_keeps_badge_opaque() {
        let width = 32;
        let base = transparent(width, width);
        let out = draw_badge(&base, width, width, AggregateLevel::Gray).expect("应当能绘制");
        assert_eq!(pixel(&out, width, 25, 25), GRAY);
        assert_eq!(pixel(&out, width, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&out, width, 31, 31), [0, 0, 0, 0]);
    }

    /// 描边随背景亮度切换：白底用深色描边，黑底/透明底用白色描边。
    #[test]
    fn stroke_contrasts_with_background() {
        let width = 32;
        // 32×32、直径 13：像素 (31,25) 距圆心 6.0，落在 [5.5, 6.5) 的描边环上。
        let rim = (31, 25);

        let dark = draw_badge(
            &solid(width, width, [0, 0, 0, 255]),
            width,
            width,
            AggregateLevel::Red,
        )
        .expect("应当能绘制");
        assert_eq!(pixel(&dark, width, rim.0, rim.1), LIGHT_STROKE);

        let light = draw_badge(
            &solid(width, width, [255, 255, 255, 255]),
            width,
            width,
            AggregateLevel::Red,
        )
        .expect("应当能绘制");
        assert_eq!(pixel(&light, width, rim.0, rim.1), DARK_STROKE);

        let transparent_base = draw_badge(
            &transparent(width, width),
            width,
            width,
            AggregateLevel::Red,
        )
        .expect("应当能绘制");
        assert_eq!(
            pixel(&transparent_base, width, rim.0, rim.1),
            LIGHT_STROKE,
            "透明底按黑底处理"
        );
    }

    /// 非 32×32 与小尺寸：都能画、都在界内、长度不变。
    #[test]
    fn handles_non_standard_sizes() {
        let cases = [
            (16u32, 16u32),
            (20, 20),
            (24, 24),
            (48, 48),
            (1, 1),
            (2, 2),
            (3, 7),
            (8, 8),
            (64, 16),
            (17, 13),
        ];
        for (width, height) in cases {
            let base = solid(width, height, [0, 0, 0, 255]);
            let out = draw_badge(&base, width, height, AggregateLevel::Green)
                .unwrap_or_else(|| panic!("{width}×{height} 应当能绘制"));
            assert_eq!(out.len(), base.len(), "{width}×{height} 长度应保持不变");

            let diameter = badge_diameter(width, height);
            assert!(diameter >= 1 && diameter <= width.min(height));
            let (x0, y0) = (width - diameter, height - diameter);
            for y in 0..height {
                for x in 0..width {
                    if x < x0 || y < y0 {
                        assert_eq!(pixel(&out, width, x, y), [0, 0, 0, 255]);
                    }
                }
            }
        }
    }

    /// 直径按宽度自适应并夹到短边。
    #[test]
    fn diameter_tracks_width() {
        assert_eq!(badge_diameter(16, 16), 6);
        assert_eq!(badge_diameter(24, 24), 10);
        assert_eq!(badge_diameter(32, 32), 13);
        assert_eq!(badge_diameter(64, 64), 26);
        assert_eq!(badge_diameter(64, 16), 16, "短边是上限");
        assert_eq!(badge_diameter(1, 1), 1, "至少 1 像素");
        assert_eq!(badge_diameter(0, 32), 0);
        assert_eq!(badge_diameter(32, 0), 0);
    }

    /// 长度/尺寸不合法一律返回 None，由调用方保留原图标。
    #[test]
    fn invalid_input_is_rejected() {
        assert!(draw_badge(&[], 0, 0, AggregateLevel::Red).is_none());
        assert!(draw_badge(&[], 0, 32, AggregateLevel::Red).is_none());
        assert!(draw_badge(&[], 32, 0, AggregateLevel::Red).is_none());
        assert!(draw_badge(&[0, 0, 0, 255], 32, 32, AggregateLevel::Red).is_none());
        let mut short = transparent(32, 32);
        short.truncate(short.len() - 4);
        assert!(draw_badge(&short, 32, 32, AggregateLevel::Red).is_none());
    }
}
