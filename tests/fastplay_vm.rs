use musadvd::author::{
    self, ButtonAction, DvdLayout, MenuButton, MenuSpec, Navigation, PlaybackOptions, Rect, TitleSpec, TitlesetSpec, TvStandard, VobSpec,
};
use musadvd::layout::{
    self, Action, Background, Canvas, CustomLayout, CustomMenu, Element, FastPlayConfig, FastPlayMenuTarget, Highlight, LayoutTitle,
    MenuRole, MenuSound,
};

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
fn fastplay_dvdauthor_xml_and_routing() {
    let b1 = MenuButton {
        name: "b1".into(),
        template_index: 0,
        location: "CUSTOM".into(),
        rect: Rect { x0: 50, y0: 400, x1: 250, y1: 450 },
        action: ButtonAction::Commands("g1 = 0; jump title 1;".into()),
        label: Some("FastPlay".into()),
        thumbnail: None,
        clip: None,
        nav: Navigation::default(),
        auto_action: false,
    };
    let b2 = MenuButton {
        name: "b2".into(),
        template_index: 1,
        location: "CUSTOM".into(),
        rect: Rect { x0: 300, y0: 400, x1: 500, y1: 450 },
        action: ButtonAction::Commands("g1 = 0; jump menu 2;".into()),
        label: Some("Main menu".into()),
        thumbnail: None,
        clip: None,
        nav: Navigation::default(),
        auto_action: false,
    };

    let fp_spec = MenuSpec {
        vob: "cm_fastplay.mpg".into(),
        buttons: vec![b1, b2],
        entry: None,
        motion: false,
        pre: Some("g1 = 0; g2 = 0;".into()),
        intro_secs: None,
        pause_secs: Some(10),
        post: Some("jump title 1;".into()),
    };

    let main_spec = MenuSpec {
        vob: "main_menu.mpg".into(),
        buttons: vec![],
        entry: Some("title".into()),
        motion: false,
        pre: None,
        intro_secs: None,
        pause_secs: None,
        post: None,
    };

    let fp_config = FastPlayConfig {
        enabled: true,
        timeout_secs: 10,
        target_title: 1,
        menu_target: FastPlayMenuTarget::VmgmMain,
        splash: None,
        audio: MenuSound::None,
    };

    let layout = DvdLayout {
        dest: "DVD".into(),
        menu_aspect: "4:3".into(),
        vmgm_menus: vec![fp_spec, main_spec],
        titlesets: vec![TitlesetSpec {
            main_menu_page: 2,
            menus: vec![],
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
        title_pre: None,
        fast_play: Some(fp_config),
    };

    let xml = author::dvdauthor_xml(&layout).unwrap();

    // 1. First Play routes to VMGM Menu 1
    assert!(xml.contains("<fpc>g1 = 0; g2 = 0; jump menu 1;</fpc>"), "xml: {xml}");

    // 2. FastPlay Menu has pause="10"
    assert!(xml.contains(r#"<vob file="cm_fastplay.mpg" pause="10"/>"#), "xml: {xml}");

    // 3. FastPlay buttons and post jump
    assert!(xml.contains("<button name=\"b1\">g1 = 0; jump title 1;</button>"), "xml: {xml}");
    assert!(xml.contains("<button name=\"b2\">g1 = 0; jump menu 2;</button>"), "xml: {xml}");
    assert!(xml.contains("<post>jump title 1;</post>"), "xml: {xml}");

    // 4. Main Menu has entry="title"
    assert!(xml.contains(r#"<pgc entry="title">"#), "xml: {xml}");
}

#[test]
fn fastplay_target_title_and_vtsm_root_routing() {
    let fp_config = FastPlayConfig {
        enabled: true,
        timeout_secs: 15,
        target_title: 2,
        menu_target: FastPlayMenuTarget::VtsmRoot,
        splash: None,
        audio: MenuSound::None,
    };

    let b1 = MenuButton {
        name: "b1".into(),
        template_index: 0,
        location: "CUSTOM".into(),
        rect: Rect { x0: 50, y0: 400, x1: 250, y1: 450 },
        action: ButtonAction::Commands("g1 = 0; jump title 2;".into()),
        label: Some("FastPlay".into()),
        thumbnail: None,
        clip: None,
        nav: Navigation::default(),
        auto_action: false,
    };
    let b2 = MenuButton {
        name: "b2".into(),
        template_index: 1,
        location: "CUSTOM".into(),
        rect: Rect { x0: 300, y0: 400, x1: 500, y1: 450 },
        action: ButtonAction::Commands("g1 = 0; jump titleset 2 menu;".into()),
        label: Some("Main menu".into()),
        thumbnail: None,
        clip: None,
        nav: Navigation::default(),
        auto_action: false,
    };

    let fp_spec = MenuSpec {
        vob: "cm_fastplay.mpg".into(),
        buttons: vec![b1, b2],
        entry: None,
        motion: false,
        pre: Some("g1 = 0; g2 = 0;".into()),
        intro_secs: None,
        pause_secs: Some(15),
        post: Some("jump title 2;".into()),
    };

    let layout = DvdLayout {
        dest: "DVD".into(),
        menu_aspect: "4:3".into(),
        vmgm_menus: vec![fp_spec],
        titlesets: vec![
            TitlesetSpec {
                main_menu_page: 1,
                menus: vec![],
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
            },
            TitlesetSpec {
                main_menu_page: 1,
                menus: vec![],
                title: TitleSpec {
                    vobs: vec![VobSpec { file: "t2.mpg".into(), chapters: vec!["0".into()] }],
                    label: "Title 2".into(),
                    aspect: "4:3".into(),
                    audio_formats: vec!["ac3".into()],
                    audio_langs: vec![Some("de".into())],
                    subtitle_langs: vec![],
                    force_subtitle: false,
                default_audio: None,
                default_subtitle: None,
                },
            },
        ],
        playback: PlaybackOptions::default(),
        tv: TvStandard::Pal,
        title_pre: None,
        fast_play: Some(fp_config),
    };

    let xml = author::dvdauthor_xml(&layout).unwrap();
    assert!(xml.contains(r#"<vob file="cm_fastplay.mpg" pause="15"/>"#), "xml: {xml}");
    assert!(xml.contains("<button name=\"b1\">g1 = 0; jump title 2;</button>"), "xml: {xml}");
    assert!(xml.contains("<button name=\"b2\">g1 = 0; jump titleset 2 menu;</button>"), "xml: {xml}");
    assert!(xml.contains("<post>jump title 2;</post>"), "xml: {xml}");
}

#[test]
fn custom_layout_fastplay_page_rendering() {
    let dir = std::env::temp_dir().join(format!("cx_test_fastplay_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let elem = Element {
        id: "btn_play".into(),
        content: layout::Content::Text {
            text: "Film abspielen".into(),
            font: "Arial".into(),
            size: 32.0,
            color: "#FFFFFF".into(),
            align: "center".into(),
            bold: false,
            italic: false,
            shadow: false,
            wrap: false,
            bg_color: None,
            bg_opacity: None,
            rx: None,
            ry: None,
            padding: None,
            shadow_color: None,
            shadow_opacity: None,
            shadow_x: None,
            shadow_y: None,
            shadow_blur: None,
            outline_color: None,
            outline_width: None,
        },
        x: 100.0,
        y: 100.0,
        w: 300.0,
        h: 50.0,
        rotation: 0.0,
        opacity: 1.0,
        action: Some(Action::PlayTitle { title: 1 }),
        highlight: None,
        group: None,
        intro: Vec::new(),
        easter_egg: false,
        nav: None,
        auto_execute: false,
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
        elements: vec![elem],
    };

    let custom_layout = CustomLayout {
        version: 1,
        canvas: Canvas::default(),
        highlight: Highlight::default(),
        animations: true,
        fast_play: Some(FastPlayConfig {
            enabled: true,
            timeout_secs: 10,
            target_title: 1,
            menu_target: FastPlayMenuTarget::VmgmMain,
            splash: None,
            audio: MenuSound::None,
        }),
        theme: None,
        settings_mode: None,
        menus: vec![main_menu],
    };

    let titles = vec![dummy_title()];
    let res = layout::build_custom_menus(&custom_layout, &titles, TvStandard::Pal, &dir, 20.0).unwrap();

    // 1. vmgm_menus[0] is FastPlay menu with pause_secs == 10 and post jump title 1
    assert_eq!(res.vmgm_menus[0].vob, "cm_fastplay.mpg");
    assert_eq!(res.vmgm_menus[0].pause_secs, Some(10));
    assert_eq!(res.vmgm_menus[0].post.as_deref(), Some("jump title 1;"));
    assert_eq!(res.vmgm_menus[0].buttons.len(), 2);

    // 2. vmgm_menus[1] is the user's main menu, with entry="title"
    assert_eq!(res.vmgm_menus[1].entry.as_deref(), Some("title"));

    // 3. FastPlay files exist on disk
    assert!(dir.join("cm_fastplay_hl.png").exists());
    assert!(dir.join("cm_fastplay_sel.png").exists());
    assert!(dir.join("cm_fastplay_spumux.xml").exists());

    let _ = std::fs::remove_dir_all(&dir);
}
