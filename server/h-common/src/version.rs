const RAW: &str = include_str!("../../../VERSION");

pub fn version() -> &'static str {
    RAW.trim()
}
