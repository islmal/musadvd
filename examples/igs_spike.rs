use convertx_rust::{bd_patch, hdmv, igs::*};
use std::path::Path;
use std::process::Command as P;
fn main() {
    let work = std::env::args().nth(1).unwrap();
    let work = Path::new(&work);
    let mk = |v: u8, w: usize, h: usize| {
        let mut px = vec![v; w * h];
        for y in 8..h - 8 { for x in 8..w - 8 { px[y * w + x] = 0; } }
        IgObject { id: 0, width: w as u16, height: h as u16, rle: rle_encode(&px, w, h) }
    };
    let mut objs = vec![];
    for (i, v) in [1u8, 2].iter().enumerate() { let mut o = mk(*v, 400, 120); o.id = i as u16; objs.push(o); }
    let mut pal = vec![[16u8, 128, 128, 0]; 256];
    let y = rgb_to_ycrcb(255, 220, 0); pal[1] = [y[0], y[1], y[2], 255];
    let y = rgb_to_ycrcb(255, 0, 0); pal[2] = [y[0], y[1], y[2], 255];
    let btn = |id, y, up, down| IgButton { id, x: 100, y, up, down, left: id, right: id, selected: 0, activated: 1, commands: vec![hdmv::jump_title(1)] };
    let m = IgMenu { width: 1280, height: 720, frame_rate_code: 1, default_selected: Some(1), palette: pal,
        buttons: vec![btn(1, 200, 1, 2), btn(2, 400, 1, 2)], objects: objs, pts: 0 };
    std::fs::write(work.join("menu.sup"), build_sup(&m)).unwrap();
    let out = work.join("disc");
    let _ = std::fs::remove_dir_all(&out);
    let ts = Path::new("C:/convertx-rust/bin/tsMuxeR.exe");
    for f in ["menu.meta", "t.meta"] {
        assert!(P::new(ts).current_dir(work).arg(f).arg(&out).status().unwrap().success());
    }
    let bdmv = out.join("BDMV");
    bd_patch::patch_menu(&bdmv, "00000", "00000", true, true).unwrap();
    // objects: 0 = title 1 (play, then back to the menu), 1 = menu, 
    let objs = [
        hdmv::MovieObject { commands: vec![hdmv::play_pl(1), hdmv::jump_object(1)], resume: false },
        hdmv::MovieObject { commands: vec![hdmv::play_pl(0)], resume: false },
    ];
    for dir in [bdmv.clone(), bdmv.join("BACKUP")] {
        std::fs::write(dir.join("MovieObject.bdmv"), hdmv::movie_object_bdmv(&objs)).unwrap();
        std::fs::write(dir.join("index.bdmv"), hdmv::index_bdmv(1, 1, &[0])).unwrap();
    }
}
