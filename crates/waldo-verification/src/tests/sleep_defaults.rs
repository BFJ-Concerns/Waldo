use super::*;

#[test]
fn portable_sleep_command_uses_path() {
    let command = wait_command();
    assert_eq!(command.get_program(), "sleep");
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["2"]);
}
