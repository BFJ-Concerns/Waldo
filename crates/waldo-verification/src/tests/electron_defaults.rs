use super::*;

fn electron(subcommand: &str, override_value: Option<&str>) -> PathBuf {
    let mut args = vec![
        "waldo-verification",
        subcommand,
        "evidence",
        "--fixture-directory",
        "fixtures",
    ];
    if subcommand == "final-campaign" {
        args.extend([
            "--waldod",
            "waldod",
            "--mcp",
            "waldo-mcp",
            "--cli",
            "waldo",
            "--corpus-directory",
            "corpus",
            "--plugin-manifest",
            "plugin.json",
        ]);
    }
    if let Some(value) = override_value {
        args.extend(["--electron", value]);
    }
    match Arguments::try_parse_from(args).unwrap().command {
        Command::ToolkitPreflight { electron, .. } | Command::FinalCampaign { electron, .. } => {
            electron
        }
        _ => unreachable!(),
    }
}

#[test]
fn portable_toolkit_electron_default_and_override() {
    assert_eq!(
        electron("toolkit-preflight", None),
        PathBuf::from("electron")
    );
    assert_eq!(
        electron("toolkit-preflight", Some("/operator/electron39")),
        PathBuf::from("/operator/electron39")
    );
}

#[test]
fn portable_campaign_electron_default_and_override() {
    assert_eq!(electron("final-campaign", None), PathBuf::from("electron"));
    assert_eq!(
        electron("final-campaign", Some("electron39")),
        PathBuf::from("electron39")
    );
}

#[test]
fn portable_electron_fixture_interpreter() {
    let fixture = include_str!("../../fixtures/interaction_electron.js");
    assert_eq!(fixture.lines().next().unwrap(), "#!/usr/bin/env electron");
}
