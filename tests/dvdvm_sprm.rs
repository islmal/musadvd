use convertx_rust::dvdvm::{compile_str, Context, Domain};

fn test_ctx() -> Context {
    Context {
        group_entries: Some(vec![0x80, 0, 0]),
        all_menu_entries: vec![0x80],
        pgc: Some([3, 3, 2]),
        titles: Some(vec![5, 3]),
    }
}

fn compile_to_hex(src: &str, domain: Domain) -> Vec<String> {
    let instructions = compile_str(src, &test_ctx(), domain).expect("compilation failed");
    instructions
        .iter()
        .map(|ins| ins.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .collect()
}

#[test]
fn test_sprm_audio_standalone() {
    // audio = 0 -> SetSTN audio stream 0 (0x80)
    assert_eq!(
        compile_to_hex("audio = 0;", Domain::Vtsm),
        vec!["5100008000000000"]
    );
    // audio = 2 -> SetSTN audio stream 2 (0x82)
    assert_eq!(
        compile_to_hex("audio = 2;", Domain::Vtsm),
        vec!["5100008200000000"]
    );
}

#[test]
fn test_sprm_subtitle_standalone() {
    // subtitle = 64 -> SetSTN subtitle stream 0 with display ON (0x80 | 0x40 = 0xC0)
    assert_eq!(
        compile_to_hex("subtitle = 64;", Domain::Vtsm),
        vec!["51000000c0000000"]
    );
    // subtitle = 65 -> SetSTN subtitle stream 1 with display ON (0x80 | 0x41 = 0xC1)
    assert_eq!(
        compile_to_hex("subtitle = 65;", Domain::Vtsm),
        vec!["51000000c1000000"]
    );
    // subtitle = 0 -> SetSTN subtitle display OFF (0x80 | 0x00 = 0x80)
    assert_eq!(
        compile_to_hex("subtitle = 0;", Domain::Vtsm),
        vec!["5100000080000000"]
    );
}

#[test]
fn test_sprm_merged_setstn_instructions() {
    // audio = 2; subtitle = 65; -> Merged into 51000082c1000000
    assert_eq!(
        compile_to_hex("audio = 2; subtitle = 65;", Domain::Vtsm),
        vec!["51000082c1000000"]
    );

    // Reverse order: subtitle = 65; audio = 2; -> Also merged into 51000082c1000000
    assert_eq!(
        compile_to_hex("subtitle = 65; audio = 2;", Domain::Vtsm),
        vec!["51000082c1000000"]
    );

    // audio = 1; subtitle = 64; -> 51000081c0000000
    assert_eq!(
        compile_to_hex("audio = 1; subtitle = 64;", Domain::Vtsm),
        vec!["51000081c0000000"]
    );

    // audio = 3; subtitle = 0; -> 5100008380000000
    assert_eq!(
        compile_to_hex("audio = 3; subtitle = 0;", Domain::Vtsm),
        vec!["5100008380000000"]
    );
}
