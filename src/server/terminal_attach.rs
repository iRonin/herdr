pub(crate) fn paste_payload_for_runtime(
    runtime: &crate::terminal::TerminalRuntime,
    text: &str,
) -> crate::pane::PreparedPaste {
    runtime.prepare_paste(text.to_owned())
}
