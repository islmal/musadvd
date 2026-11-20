use musadvd::author::{self, DvdLayout, PlaybackOptions, TitleSpec, TitlesetSpec, TvStandard, VobSpec};
use musadvd::layout::{
    self, Action, Background, Canvas, CustomLayout, CustomMenu, Element, Highlight, LayoutTitle, MenuRole, MenuSound,
    TransitionVideo, VideoAssetSource,
};
use std::path::PathBuf;

fn dummy_title() -> LayoutTitle {
    LayoutTitle {
        label: "Title 1".into(),
        chapters: 3,
        duration_secs: 120.0,
        clip: musadvd::author::ClipSource { path: "t1.mpg".into(), start_secs: 0.0, has_audio: true },
        audio_sources: vec![0],
        subtitle_sources: vec![0],
        audio_labels: vec!["Stereo".into()],
        subtitle_labels: vec!["Deutsch".into()],
    }
}

#[test]
fn menu_transition_bridge_pgc_vmgm() {
    let dir = std::env::temp_dir().join(format!("cx_test_bridge_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let trans = TransitionVideo {
        video: VideoAssetSource::File {
            path: PathBuf::from("transition.mpg"),
            in_point_secs: None,
        },
        duration_secs: 2.0,
        audio: MenuSound::None,
    };

    let btn_to_m2 = Element {
        id: "btn_m2".into(),
        content: layout::Content::Text {
            text: "Zu Menü 2".into(),
            font: "Arial".into(),
            size: 32.0,
            color: "#FFFFFF".into(),
            align: "center".into(),
            bold: false,
            italic: false,
            shadow: false,
            wrap: false,
        },
        x: 100.0,
        y: 100.0,
        w: 250.0,
        h: 50.0,
        rotation: 0.0,
        opacity: 1.0,
        action: Some(Action::Menu {
            menu: "menu2".into(),
            transition: Some(trans.clone()),
        }),
        highlight: None,
        group: None,
        intro: Vec::new(),
        easter_egg: false,
    };

    let menu1 = CustomMenu {
        id: "menu1".into(),
        name: "Menü 1".into(),
        role: MenuRole::Title,
        title: None,
        background: Background::default(),
        duration_secs: None,
        audio: MenuSound::None,
        intro_secs: None,
        group: None,
        elements: vec![btn_to_m2],
    };

    let menu2 = CustomMenu {
        id: "menu2".into(),
        name: "Menü 2".into(),
        role: MenuRole::Title,
        title: None,
        background: Background::default(),
        duration_secs: None,
        audio: MenuSound::None,
        intro_secs: None,
        group: None,
        elements: vec![],
    };

    let custom_layout = CustomLayout {
        version: 1,
        canvas: Canvas::default(),
        highlight: Highlight::default(),
        animations: true,
        fast_play: None,
        theme: None,
        settings_mode: None,
        menus: vec![menu1, menu2],
    };

    let titles = vec![dummy_title()];
    let res = layout::build_custom_menus(&custom_layout, &titles, TvStandard::Pal, &dir, 20.0).unwrap();

    // 1. Assert that Menu 1 Button 1 jumps to the Bridge PGC (jump menu 3;)
    assert_eq!(res.vmgm_menus[0].buttons.len(), 1);
    let ctx = author::CommandContext { domain: author::Domain::Vmgm, menu: 1 };
    assert_eq!(res.vmgm_menus[0].buttons[0].action.command(ctx), "jump menu 3;");

    // 2. Assert that the Bridge PGC contains transition.mpg with pauselen == 0 and no buttons
    assert_eq!(res.vmgm_menus.len(), 3);
    let bridge = &res.vmgm_menus[2];
    assert_eq!(bridge.vob, "transition.mpg");
    assert_eq!(bridge.pause_secs, Some(0));
    assert!(bridge.buttons.is_empty());

    // 3. Assert that the Bridge PGC <post> command executes jump menu 2;
    assert_eq!(bridge.post.as_deref(), Some("jump menu 2;"));

    // 4. Inspect dvdauthor XML output
    let dvd_layout = DvdLayout {
        dest: "DVD".into(),
        menu_aspect: "4:3".into(),
        vmgm_menus: res.vmgm_menus.clone(),
        titlesets: vec![TitlesetSpec {
            main_menu_page: res.main_menu_pages[0],
            menus: res.titleset_menus[0].clone(),
            title: TitleSpec {
                vobs: vec![VobSpec { file: "t1.mpg".into(), chapters: vec!["0".into()] }],
                label: "Title 1".into(),
                aspect: "4:3".into(),
                audio_formats: vec!["ac3".into()],
                audio_langs: vec![Some("de".into())],
                subtitle_langs: vec![],
                force_subtitle: false,
                default_audio: None,
                default_subtitle: None,
            },
        }],
        playback: PlaybackOptions::default(),
        tv: TvStandard::Pal,
        title_pre: Some(res.title_pre),
        fast_play: None,
    };

    let xml = author::dvdauthor_xml(&dvd_layout).unwrap();
    assert!(xml.contains("<button name=\"b1\">jump menu 3;</button>"), "xml: {xml}");
    assert!(xml.contains(r#"<vob file="transition.mpg" pause="0"/>"#), "xml: {xml}");
    assert!(xml.contains("<post>jump menu 2;</post>"), "xml: {xml}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn menu_transition_cross_domain_and_vtsm() {
    let dir = std::env::temp_dir().join(format!("cx_test_bridge_vtsm_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let trans_intra = TransitionVideo {
        video: VideoAssetSource::File {
            path: PathBuf::from("intra_trans.mpg"),
            in_point_secs: None,
        },
        duration_secs: 1.5,
        audio: MenuSound::None,
    };

    let trans_to_vmgm = TransitionVideo {
        video: VideoAssetSource::File {
            path: PathBuf::from("to_main.mpg"),
            in_point_secs: None,
        },
        duration_secs: 2.5,
        audio: MenuSound::None,
    };

    // Root 1 has 2 buttons:
    // 1. Linking to Root 2 with intra-VTSM transition
    // 2. Linking to Title Menu with cross-domain transition
    let btn_root2 = Element {
        id: "b_root2".into(),
        content: layout::Content::Text {
            text: "Root 2".into(),
            font: "Arial".into(),
            size: 24.0,
            color: "#FFFFFF".into(),
            align: "center".into(),
            bold: false,
            italic: false,
            shadow: false,
            wrap: false,
        },
        x: 50.0,
        y: 50.0,
        w: 200.0,
        h: 40.0,
        rotation: 0.0,
        opacity: 1.0,
        action: Some(Action::Menu {
            menu: "root2".into(),
            transition: Some(trans_intra),
        }),
        highlight: None,
        group: None,
        intro: Vec::new(),
        easter_egg: false,
    };

    let btn_main = Element {
        id: "b_main".into(),
        content: layout::Content::Text {
            text: "Main menu".into(),
            font: "Arial".into(),
            size: 24.0,
            color: "#FFFFFF".into(),
            align: "center".into(),
            bold: false,
            italic: false,
            shadow: false,
            wrap: false,
        },
        x: 50.0,
        y: 120.0,
        w: 200.0,
        h: 40.0,
        rotation: 0.0,
        opacity: 1.0,
        action: Some(Action::Menu {
            menu: "main".into(),
            transition: Some(trans_to_vmgm),
        }),
        highlight: None,
        group: None,
        intro: Vec::new(),
        easter_egg: false,
    };

    let main_menu = CustomMenu {
        id: "main".into(),
        name: "Main menu".into(),
        role: MenuRole::Title,
        title: None,
        background: Background::default(),
        duration_secs: None,
        audio: MenuSound::None,
        intro_secs: None,
        group: None,
        elements: vec![],
    };

    let root1 = CustomMenu {
        id: "root1".into(),
        name: "Root 1".into(),
        role: MenuRole::Root,
        title: Some(1),
        background: Background::default(),
        duration_secs: None,
        audio: MenuSound::None,
        intro_secs: None,
        group: None,
        elements: vec![btn_root2, btn_main],
    };

    let root2 = CustomMenu {
        id: "root2".into(),
        name: "Root 2".into(),
        role: MenuRole::Root,
        title: Some(1),
        background: Background::default(),
        duration_secs: None,
        audio: MenuSound::None,
        intro_secs: None,
        group: None,
        elements: vec![],
    };

    let custom_layout = CustomLayout {
        version: 1,
        canvas: Canvas::default(),
        highlight: Highlight::default(),
        animations: true,
        fast_play: None,
        theme: None,
        settings_mode: None,
        menus: vec![main_menu, root1, root2],
    };

    let titles = vec![dummy_title()];
    let res = layout::build_custom_menus(&custom_layout, &titles, TvStandard::Pal, &dir, 20.0).unwrap();

    // VMGM menus: Main Menu (PGC 1) + VMGM Bridge for to_main (PGC 2)
    assert_eq!(res.vmgm_menus.len(), 2);
    assert_eq!(res.vmgm_menus[0].vob, "cm01.mpg");
    assert_eq!(res.vmgm_menus[1].vob, "to_main.mpg");
    assert_eq!(res.vmgm_menus[1].post.as_deref(), Some("jump menu 1;"));

    // VTSM menus for Title 1:
    // Root 1 is PGC 1, Root 2 is PGC 2, Intra-VTSM Bridge is PGC 3!
    let ts_menus = &res.titleset_menus[0];
    assert_eq!(ts_menus.len(), 3);
    assert_eq!(ts_menus[0].vob, "cm02.mpg");
    assert_eq!(ts_menus[1].vob, "cm03.mpg");
    assert_eq!(ts_menus[2].vob, "intra_trans.mpg");
    assert_eq!(ts_menus[2].post.as_deref(), Some("jump menu 2;"));

    // Root 1 Button 1 (intra-VTSM) jumps to VTSM Bridge PGC 3
    let ctx = author::CommandContext { domain: author::Domain::Vtsm, menu: 1 };
    assert_eq!(ts_menus[0].buttons[0].action.command(ctx), "jump menu 3;");

    // Root 1 Button 2 (cross-domain to VMGM) jumps to VMGM Bridge PGC 2
    assert_eq!(ts_menus[0].buttons[1].action.command(ctx), "jump vmgm menu 2;");

    let _ = std::fs::remove_dir_all(&dir);
}
