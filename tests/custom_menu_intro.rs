use convertx_rust::author::{self, DvdLayout, PlaybackOptions, TitleSpec, TitlesetSpec, TvStandard, VobSpec};
use convertx_rust::layout::{
    self, Action, Background, Canvas, CustomLayout, CustomMenu, Element, ElementKey, Highlight, LayoutTitle, MenuRole, MenuSound,
};

fn dummy_title() -> LayoutTitle {
    LayoutTitle {
        label: "Title 1".into(),
        chapters: 3,
        duration_secs: 120.0,
        clip: convertx_rust::author::ClipSource { path: "t1.mpg".into(), start_secs: 0.0, has_audio: true },
        audio_sources: vec![0],
        subtitle_sources: vec![0],
        audio_labels: vec!["Stereo".into()],
        subtitle_labels: vec!["Deutsch".into()],
    }
}

#[test]
fn custom_menu_two_cell_intro_and_closed_gop_structure() {
    let dir = std::env::temp_dir().join(format!("cx_test_custom_intro_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let elem = Element {
        id: "btn_play".into(),
        content: layout::Content::Text {
            text: "Starten".into(),
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
        w: 300.0,
        h: 50.0,
        rotation: 0,
        opacity: 1.0,
        action: Some(Action::PlayTitle { title: 1 }),
        highlight: None,
        group: None,
        intro: vec![
            ElementKey { t: 0.0, x: -300.0, y: 100.0, w: 300.0, h: 50.0, opacity: 0.0 },
            ElementKey { t: 3.0, x: 100.0, y: 100.0, w: 300.0, h: 50.0, opacity: 1.0 },
        ],
        easter_egg: false,
    };

    let menu = CustomMenu {
        id: "main".into(),
        name: "Intro Menu".into(),
        role: MenuRole::Title,
        title: None,
        background: Background::default(),
        duration_secs: Some(15.0),
        audio: MenuSound::None,
        intro_secs: Some(3.0),
        elements: vec![elem],
    };

    let layout = CustomLayout {
        version: 1,
        canvas: Canvas::default(),
        highlight: Highlight::default(),
        animations: true,
        fast_play: None,
        menus: vec![menu],
    };

    let titles = vec![dummy_title()];
    let res = layout::build_custom_menus(&layout, &titles, TvStandard::Pal, &dir, 15.0).unwrap();

    // 1. files.intro is populated and aligned to whole DVD GOPs (PAL: 25 fps, 15 frames/GOP -> 75 frames for 3s)
    let (_, files) = &res.pages[0];
    let intro = files.intro.as_ref().expect("intro should be present");
    assert_eq!(intro.frames, 75);
    assert_eq!(intro.secs, 3.0);
    assert_eq!(res.vmgm_menus[0].intro_secs, Some(3.0));

    // 2. spumux.xml starts at 3.0s
    let xml = std::fs::read_to_string(&files.spumux_xml).unwrap();
    assert!(xml.contains(r#"<spu start="00:00:03.000""#), "xml: {xml}");

    // 3. DVD layout produces two-cell VOB structure with cell 2 looping
    let dvd_layout = DvdLayout {
        dest: "DVD".into(),
        menu_aspect: "4:3".into(),
        vmgm_menus: res.vmgm_menus,
        titlesets: vec![TitlesetSpec {
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
        }],
        playback: PlaybackOptions::default(),
        tv: TvStandard::Pal,
        title_pre: None,
        fast_play: None,
    };
    let dvd_xml = author::dvdauthor_xml(&dvd_layout).unwrap();
    assert!(dvd_xml.contains(r#"<cell start="0" end="3.000" program="1"/>"#), "dvd_xml: {dvd_xml}");
    assert!(dvd_xml.contains(r#"<cell start="3.000"/>"#), "dvd_xml: {dvd_xml}");
    assert!(dvd_xml.contains("<post>jump cell 2;</post>"), "dvd_xml: {dvd_xml}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn easter_egg_transparent_mask() {
    let dir = std::env::temp_dir().join(format!("cx_test_easter_egg_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let normal_btn = Element {
        id: "btn_normal".into(),
        content: layout::Content::Text {
            text: "Film".into(),
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
        w: 200.0,
        h: 50.0,
        rotation: 0,
        opacity: 1.0,
        action: Some(Action::PlayTitle { title: 1 }),
        highlight: None,
        group: None,
        intro: Vec::new(),
        easter_egg: false,
    };

    let easter_egg_btn = Element {
        id: "btn_secret".into(),
        content: layout::Content::Text {
            text: "Geheim".into(),
            font: "Arial".into(),
            size: 32.0,
            color: "#FFFFFF".into(),
            align: "center".into(),
            bold: false,
            italic: false,
            shadow: false,
            wrap: false,
        },
        x: 400.0,
        y: 100.0,
        w: 200.0,
        h: 50.0,
        rotation: 0,
        opacity: 1.0,
        action: Some(Action::CustomVm { script: "g4 = 42; jump title 1;".into() }),
        highlight: None,
        group: None,
        intro: Vec::new(),
        easter_egg: true,
    };

    let menu = CustomMenu {
        id: "main".into(),
        name: "Egg Menu".into(),
        role: MenuRole::Title,
        title: None,
        background: Background::default(),
        duration_secs: Some(10.0),
        audio: MenuSound::None,
        intro_secs: None,
        elements: vec![normal_btn, easter_egg_btn],
    };

    let layout = CustomLayout {
        version: 1,
        canvas: Canvas::default(),
        highlight: Highlight::default(),
        animations: true,
        fast_play: None,
        menus: vec![menu],
    };

    let titles = vec![dummy_title()];
    let res = layout::build_custom_menus(&layout, &titles, TvStandard::Pal, &dir, 10.0).unwrap();

    // 1. Both buttons exist in the authoring spec and spumux.xml
    assert_eq!(res.vmgm_menus[0].buttons.len(), 2);
    let xml = std::fs::read_to_string(&res.pages[0].1.spumux_xml).unwrap();
    assert!(xml.contains(r#"<button name="b1""#));
    assert!(xml.contains(r#"<button name="b2""#));

    // 2. Button 2 has navigation linking to Button 1
    assert!(xml.contains(r#"left="b1""#) || xml.contains(r#"right="b2""#));

    // 3. Highlight PNG: Bounding box of easter egg button (x: 400..600, y: 100..150) must have 0 non-transparent pixels!
    let hl_img = image::open(&res.pages[0].1.highlight).unwrap().to_rgba8();
    let (fw, fh) = (720, 576);
    let egg_x0 = (400.0 * fw as f64 / 720.0).round() as u32;
    let egg_y0 = (100.0 * fh as f64 / 576.0).round() as u32;
    let egg_x1 = egg_x0 + (200.0 * fw as f64 / 720.0).round() as u32;
    let egg_y1 = egg_y0 + (50.0 * fh as f64 / 576.0).round() as u32;

    for y in egg_y0..egg_y1 {
        for x in egg_x0..egg_x1 {
            let pixel = hl_img.get_pixel(x, y);
            assert_eq!(pixel[3], 0, "Easter egg button should have 0 alpha/highlight at ({x}, {y})");
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}
