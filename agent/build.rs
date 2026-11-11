include!("../deploy/windows/resources.rs");

fn main() {
    // No manifest: the agent sets its DPI awareness at run time.
    embed_windows_resources("flow-agent", "FlowCursor agent", None);
}
