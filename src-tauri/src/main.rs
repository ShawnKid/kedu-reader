//! 桌面应用入口（Windows 下 release 隐藏控制台窗口）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    kedu_reader_lib::run();
}
