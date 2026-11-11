// Release builds are GUI-subsystem programs: no console window. Debug builds
// keep the console so you can watch the log while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! `clipapp.exe`                 core: tray icon, hotkeys, replay buffer
//! `clipapp.exe --ui [--tab T] [--clip FILE] [--minimized]`
//!                               the window (started by the core on demand)
//! `clipapp.exe --detect`        print hardware detection + recommendation
//! `clipapp.exe --quit`          ask the running instance to exit

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = if args.iter().any(|a| a == "--ui") {
        clipapp::ui::run(clipapp::ui::UiArgs::parse(&args))
    } else if args.iter().any(|a| a == "--detect") {
        detect()
    } else if args.iter().any(|a| a == "--quit") {
        if !clipapp::app::quit_running_instance() {
            eprintln!("ClipApp is not running");
        }
        Ok(())
    } else {
        clipapp::app::run_core()
    };
    if let Err(e) = result {
        log::error!("fatal: {e:#}");
        clipapp::win::error_box(&format!("{e:#}"));
        std::process::exit(1);
    }
}

fn detect() -> anyhow::Result<()> {
    clipapp::win::set_dpi_awareness();
    let hw = clipapp::tune::detect();
    println!("{}", serde_json::to_string_pretty(&hw)?);
    let rec = clipapp::tune::recommend(&hw);
    println!("\nRecommended: {}  ({})", rec.preset.label(), rec.why.join(" "));
    for p in clipapp::tune::Preset::AUTO {
        let plan = clipapp::tune::plan(&hw, p);
        println!("\n{}:", p.label());
        for line in plan.why {
            println!("  - {line}");
        }
    }
    Ok(())
}
