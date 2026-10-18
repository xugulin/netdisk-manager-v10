//! 示例：B 站 XML 弹幕 → 车道布局 → ASS 弹幕轨（含时间窗分片）。
//!
//! 运行：`cargo run -p wp-danmaku --example ass_export`

use wp_danmaku::Comment;
use wp_danmaku::{
    export_bilibili_xml, generate_ass, generate_ass_windowed, layout, parse_bilibili_xml,
    DanmakuMode, LayoutConfig,
};

fn main() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<i>
  <d p="1.50,1,25,16777215,0,0,user,1">普通滚动弹幕</d>
  <d p="2.00,5,25,16711680,0,0,user,2">顶部红色</d>
  <d p="2.20,4,18,255,0,0,user,3">底部小字</d>
  <d p="3.00,1,25,65280,0,0,user,4">绿色滚动</d>
</i>"#;
    let mut doc = parse_bilibili_xml(xml).expect("解析 B 站 XML");
    // 再补几条同屏弹幕，观察车道分配
    for i in 0..10 {
        doc.comments.push(Comment::new(
            2000,
            DanmakuMode::Scroll,
            format!("同屏弹幕{i}"),
        ));
    }
    doc.normalize();
    let cfg = LayoutConfig::hd1080();
    let placed = layout(&doc.comments, &cfg);
    eprintln!(
        "输入 {} 条 → 布局 {} 条（车道数 {}）",
        doc.comments.len(),
        placed.len(),
        wp_danmaku::geometry(&cfg).lane_count
    );
    let ass = generate_ass(&placed, &cfg);
    print!("{ass}");

    // 分片：只导出 0~3 秒窗口（Mode B 分片 sub-reload 用）
    let chunk = generate_ass_windowed(&placed, &cfg, 0, 3000);
    eprintln!(
        "时间窗 [0,3000) 内事件 {} 条",
        chunk.matches("Dialogue:").count()
    );
    // 导出回 B 站 XML（可再次被 parse_bilibili_xml 解析）
    eprintln!("导出 XML 字节数：{}", export_bilibili_xml(&doc).len());
}
