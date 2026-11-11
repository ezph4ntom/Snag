// Release builds are GUI-subsystem programs: no console window. Debug builds
// keep the console so you can watch the log while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! `snag.exe`                 core: tray icon, hotkeys, replay buffer
//! `snag.exe --ui [--tab T] [--clip FILE] [--minimized]`
//!                               the window (started by the core on demand)
//! `snag.exe --detect`        print hardware detection + recommendation
//! `snag.exe --quit`          ask the running instance to exit

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let result = if args.iter().any(|a| a == "--ui") {
        snag::ui::run(snag::ui::UiArgs::parse(&args))
    } else if args.iter().any(|a| a == "--detect") {
        detect()
    } else if args.iter().any(|a| a == "--quit") {
        if !snag::app::quit_running_instance() {
            eprintln!("Snag is not running");
        }
        Ok(())
    } else {
        snag::app::run_core(args.iter().any(|a| a == "--replace"))
    };
    if let Err(e) = result {
        log::error!("fatal: {e:#}");
        snag::win::error_box(&format!("{e:#}"));
        std::process::exit(1);
    }
}

fn detect() -> anyhow::Result<()> {
    snag::win::set_dpi_awareness();
    let hw = snag::tune::detect();
    println!("{}", serde_json::to_string_pretty(&hw)?);
    let rec = snag::tune::recommend(&hw);
    println!("\nRecommended: {}  ({})", rec.preset.label(), rec.why.join(" "));
    for p in snag::tune::Preset::AUTO {
        let plan = snag::tune::plan(&hw, p);
        println!("\n{}:", p.label());
        for line in plan.why {
            println!("  - {line}");
        }
    }
    Ok(())
}
