use std::path::PathBuf;
#[allow(dead_code)]
#[path = "../../../crates/robot-health/src/proc.rs"]
mod proc;
mod viz_case {
    use std::path::PathBuf;
    type Error = Box<dyn std::error::Error>;
    pub struct Options { pub report: Option<PathBuf> }
    #[derive(Default)]
    struct Report { output_status: String, error: Option<String> }
    // 仅替代 JSON 序列化；下方 run 函数从当前源码逐字提取，真实文件写入不替代。
    mod serde_json {
        pub fn to_vec_pretty(report: &super::Report) -> std::io::Result<Vec<u8>> {
            Ok(format!("{} {:?}", report.output_status, report.error).into_bytes())
        }
    }
    fn run_inner(_: &Options, _: &mut Report) -> Result<(), Error> {
        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "原始运行失败: source fixture invalid").into())
    }
pub fn run(options: &Options) -> Result<(), Error> {
    let mut report = Report::default();
    let result = run_inner(options, &mut report);
    match &result {
        Ok(()) => report.output_status = "success".into(),
        Err(error) => {
            report.output_status = "error".into();
            report.error = Some(error.to_string());
        }
    }
    if let Some(path) = &options.report {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    }
    result
}

}
fn main() {
    // 调用真实 /proc 解析器制造明确 InvalidData，再执行当前 monitor 的投影表达式。
    let mem = proc::parse_meminfo("broken");
    let load = proc::parse_loadavg("broken");
    assert!(mem.is_err() && load.is_err());
    let mem_used_pct = mem.as_ref().map_or(0.0, proc::MemInfo::used_pct);
    let load_1m = load.as_ref().map_or(0.0, |l| l.load_1m);
    println!("health: mem_error={} load_error={} -> mem_used_pct={mem_used_pct} load_1m={load_1m}", mem.as_ref().unwrap_err(), load.as_ref().unwrap_err());
    assert_eq!(mem_used_pct, 0.0); assert_eq!(load_1m, 0.0);
    let root: PathBuf = std::env::temp_dir().join(format!("robot-system-viz-error-review-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let original = viz_case::run(&viz_case::Options { report: Some(root.join("report.txt")) }).unwrap_err().to_string();
    let replaced = viz_case::run(&viz_case::Options { report: Some(root.clone()) }).unwrap_err().to_string();
    println!("viz writable report: {original}");
    println!("viz directory report: {replaced}");
    assert!(original.contains("source fixture invalid"));
    assert!(!replaced.contains("source fixture invalid"));
    std::fs::remove_dir_all(root).unwrap();
}
