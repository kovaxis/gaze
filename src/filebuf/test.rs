use crate::{
    filebuf::{
        linemap::{decode_utf8, LineMap, LineMapper},
        sparse::SparseData,
        LoadedData,
    },
    prelude::*,
};

struct TestInst {
    loaded: Mutex<LoadedData>,
    linemapper: LineMapper,
}

fn init(fsize: i64, max_mem: usize) -> TestInst {
    let font = FontArc::try_from_vec(fs::read("font.ttf").unwrap()).unwrap();
    TestInst {
        loaded: Mutex::new(LoadedData {
            linemap: LineMap::new(fsize),
            data: SparseData::new(fsize, usize::MAX, 1024, 1024),
            hot: default(),
            sel: None,
            pending_sel_copy: false,
            warn_time: None,
        }),
        linemapper: LineMapper::new(font, fsize, max_mem, 1024),
    }
}

use rand::{seq::SliceRandom, Rng, SeedableRng};

type TestRng = rand_xoshiro::Xoshiro256StarStar;

struct SegSpec {
    start: i64,
    end: i64,
    abs_x_since: i64,
    start_x: f64,
    end_x: f64,
    abs_y: bool,
    start_y: i64,
    end_y: i64,
    widest: f64,
    rel_width: f64,
}

fn assert_linemap_segs_eq(t: &TestInst, segs: Vec<SegSpec>) {
    let lm = &t.loaded.lock().linemap;
    assert_eq!(lm.segments.len(), segs.len());
    for (got, ex) in lm.segments.iter().zip(segs.iter()) {
        assert_eq!(got.start, ex.start);
        assert_eq!(got.end, ex.end);
        assert_eq!(got.anchors.front().unwrap().offset, ex.start);
        assert_eq!(got.anchors.back().unwrap().offset, ex.end);
        assert_eq!(got.start == 0, ex.abs_y);
        if ex.abs_x_since > ex.end {
            assert_eq!(got.first_absolute, got.anchors.len());
        } else {
            assert!(got.anchors[got.first_absolute].offset >= ex.abs_x_since);
            if got.first_absolute > 0 {
                assert!(got.anchors[got.first_absolute - 1].offset < ex.abs_x_since);
            }
        }
        assert_eq!(got.widest_line, ex.widest);
        assert_eq!(got.rel_width, ex.rel_width);
        assert_eq!(got.anchors.front().unwrap().x(got), ex.start_x);
        assert_eq!(got.anchors.back().unwrap().x(got), ex.end_x);
        assert_eq!(got.anchors.front().unwrap().y(got), ex.start_y);
        assert_eq!(got.anchors.back().unwrap().y(got), ex.end_y);
    }
}

fn assert_sparse_data_eq(t: &TestInst, segs: Vec<(i64, Vec<u8>)>) {
    let sd = &t.loaded.lock().data;
    assert_eq!(sd.segments.len(), segs.len());
    for (got, ex) in sd.segments.iter().zip(segs.iter()) {
        assert_eq!(got.offset, ex.0);
        assert_eq!(&got.data[..], &ex.1);
    }
}

fn assert_full_data_loaded(t: &TestInst, data: &[u8]) {
    let mut x = 0.;
    let mut y = 0;
    let mut w = 0f64;
    let mut idx = 0;
    while idx < data.len() {
        let (c, adv) = decode_utf8(&data[idx..]);
        let c_i = idx;
        idx += adv;
        let x_i = x;
        match c.unwrap_or(LineMapper::REPLACEMENT_CHAR) {
            LineMapper::NEWLINE => {
                w = w.max(x);
                x = 0.;
                y += 1;
                println!("char {} is newline", c_i);
            }
            c => {
                x += t.linemapper.advance_for(c);
                println!("char [{}, {}) uses x [{}, {})", c_i, idx, x_i, x);
            }
        }
    }
    w = w.max(x);
    assert_linemap_segs_eq(
        &t,
        vec![SegSpec {
            start: 0,
            end: data.len() as i64,
            abs_x_since: 0,
            start_x: 0.,
            end_x: x,
            abs_y: true,
            start_y: 0,
            end_y: y,
            widest: w,
            rel_width: 0.,
        }],
    );
    assert_sparse_data_eq(&t, vec![(0, data.to_vec())]);
}

/// The ranges should cover all data.
fn test_in_order(
    data: &[u8],
    max_mem: usize,
    ranges: impl IntoIterator<Item = ops::Range<i64>>,
) -> TestInst {
    let t = init(data.len() as i64, max_mem);
    for r in ranges {
        let subdata = &data[r.start as usize..r.end as usize];
        t.linemapper.process_data(&t.loaded, r.start, subdata);
        SparseData::insert_data(&t.loaded, r.start, subdata.to_vec());
    }
    println!("data:\n{}\n", String::from_utf8_lossy(data));
    println!("{:?}", t.loaded.lock().linemap);
    assert_full_data_loaded(&t, data);
    t
}

fn rand_binary(seed: u64, len: usize) -> Vec<u8> {
    let mut data = vec![0; len];
    let mut rng = TestRng::seed_from_u64(seed);
    rng.fill(&mut data[..]);
    data
}

fn rand_ascii(seed: u64, len: usize) -> Vec<u8> {
    let mut data = rand_binary(seed, len);
    for b in data.iter_mut() {
        *b = *b & 0x7F;
    }
    data
}

fn rand_utf8(seed: u64, len: usize) -> Vec<u8> {
    let mut data = String::new();
    let mut rng = TestRng::seed_from_u64(seed);
    while data.len() < len {
        let c = if rng.gen_bool(0.01) { '\n' } else { rng.gen() };
        data.push(c);
        if data.len() > len {
            data.pop();
        }
    }
    data.into_bytes()
}

fn rand_utf8_blocks(mut seed: u64, block_size: usize, block_count: usize) -> Vec<u8> {
    let mut data = Vec::new();
    for _ in 0..block_count {
        data.append(&mut rand_utf8(seed, block_size));
        seed = seed.wrapping_add(0xdeadbeefdeadbeef);
    }
    data
}

#[test]
fn sequential() {
    test_in_order(
        &rand_utf8_blocks(0xdaba, 256, 16),
        2 * 1024,
        (0..16).map(|i| 256 * i..256 * (i + 1)),
    );
}

#[test]
fn sequential_rev() {
    test_in_order(
        &rand_utf8_blocks(0xdabb, 256, 16),
        2 * 1024,
        (0..16).map(|i| 256 * i..256 * (i + 1)).rev(),
    );
}

#[test]
fn checkers() {
    test_in_order(
        &rand_utf8_blocks(0xdabc, 64, 256),
        2 * 1024,
        (0..128)
            .map(|i| 64 * (2 * i)..64 * (2 * i + 1))
            .chain((0..128).map(|i| 64 * (2 * i + 1)..64 * (2 * i + 2))),
    );
}

#[test]
fn shuffled() {
    let mut rng = TestRng::seed_from_u64(0xdeadbeeeee);
    let mut blocks = vec![0; 256];
    for (i, b) in blocks.iter_mut().enumerate() {
        *b = i as i64;
    }
    blocks.shuffle(&mut rng);

    test_in_order(
        &rand_utf8_blocks(0xdabd, 64, 256),
        2 * 1024,
        blocks.iter().map(|&i| 64 * i..64 * (i + 1)),
    );
}

#[test]
fn unequal() {
    let mut rng = TestRng::seed_from_u64(0xabcdef);
    let size: i64 = 64 * 256;
    let mut splits = vec![];
    for _ in 0..255 {
        splits.push(rng.gen_range(1..size));
    }
    splits.push(0);
    splits.push(size);
    splits.sort();

    test_in_order(
        &rand_ascii(0xdabe, size as usize),
        2 * 1024,
        (0..256).map(|i| splits[i]..splits[i + 1]),
    );
}

#[test]
fn unequal_shuffled() {
    let mut rng = TestRng::seed_from_u64(0xabcdef);
    let size: i64 = 64 * 256;
    let mut splits = vec![];
    for _ in 0..255 {
        splits.push(rng.gen_range(1..size));
    }
    splits.push(0);
    splits.push(size);
    splits.sort();

    let mut order = vec![0; 256];
    for (i, b) in order.iter_mut().enumerate() {
        *b = i;
    }
    order.shuffle(&mut rng);

    test_in_order(
        &rand_ascii(0xdabf, size as usize),
        2 * 1024,
        order.iter().map(|&i| splits[i]..splits[i + 1]),
    );
}

#[test]
fn binary_babysteps() {
    let data = rand_binary(0xbadeefdab, 32 * 1024);
    let t = init(data.len() as i64, 2 * 1024);
    let mut rsize = 1;
    loop {
        let ((l, r), _store) = t.loaded.lock().get_range_to_load(rsize, 100000, 0);
        if l >= r {
            break;
        }
        let old = t
            .loaded
            .lock()
            .linemap
            .segments
            .last()
            .map(|s| s.end)
            .unwrap_or(0);
        t.linemapper
            .process_data(&t.loaded, l, &data[l as usize..r as usize]);
        SparseData::insert_data(&t.loaded, l, data[l as usize..r as usize].to_vec());
        if old
            == t.loaded
                .lock()
                .linemap
                .segments
                .last()
                .map(|s| s.end)
                .unwrap_or(0)
        {
            rsize += 1;
        } else {
            rsize = 1;
        }
        assert!(rsize <= 4);
    }
    println!("data:\n{}\n", String::from_utf8_lossy(&data));
    println!("{:?}", t.loaded.lock().linemap);
    assert_full_data_loaded(&t, &data);
}

#[test]
fn binary_babysteps_rev() {
    let data = rand_binary(0xbadeefdab, 32 * 1024);
    let fsize = data.len() as i64;
    let t = init(fsize, 2 * 1024);
    t.loaded.lock().hot.corner.base_offset = fsize - 1;
    let mut rsize = 1;
    loop {
        let ((l, r), _store) = t.loaded.lock().get_range_to_load(rsize, 100000, 0);
        if l >= r {
            break;
        }
        let old = t
            .loaded
            .lock()
            .linemap
            .segments
            .first()
            .map(|s| s.start)
            .unwrap_or(fsize);
        t.linemapper
            .process_data(&t.loaded, l, &data[l as usize..r as usize]);
        SparseData::insert_data(&t.loaded, l, data[l as usize..r as usize].to_vec());
        if old
            == t.loaded
                .lock()
                .linemap
                .segments
                .first()
                .map(|s| s.start)
                .unwrap_or(fsize)
        {
            rsize += 1;
        } else {
            rsize = 1;
        }
        assert!(rsize <= 4);
    }
    println!("data:");
    for (i, b) in data.iter().enumerate() {
        println!("{:02}: {:03} = {:02x} = {:08b}", i, b, b, b);
    }
    println!("data:\n{}\n", String::from_utf8_lossy(&data));
    println!("{:?}", t.loaded.lock().linemap);
    assert_full_data_loaded(&t, &data);
}
