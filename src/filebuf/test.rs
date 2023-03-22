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
            data: SparseData::new(fsize),
            hot_offset: 0,
        }),
        linemapper: LineMapper::new(font, max_mem, fsize),
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
        assert_eq!(
            got.anchors
                .front()
                .unwrap()
                .x(got.base_x_relative, got.first_absolute <= 0),
            ex.start_x
        );
        assert_eq!(
            got.anchors.back().unwrap().x(
                got.base_x_relative,
                got.first_absolute <= got.anchors.len() - 1
            ),
            ex.end_x
        );
        assert_eq!(got.anchors.front().unwrap().y(got.base_y), ex.start_y);
        assert_eq!(got.anchors.back().unwrap().y(got.base_y), ex.end_y);
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
    println!("{:?}", t.loaded.lock().linemap);
    let mut x = 0.;
    let mut y = 0;
    let mut idx = 0;
    while idx < data.len() {
        let (c, adv) = decode_utf8(&data[idx..]);
        idx += adv;
        match c.unwrap_or(LineMapper::REPLACEMENT_CHAR) {
            '\n' => {
                x = 0.;
                y += 1;
            }
            c => {
                x += t.linemapper.advance_for(c);
            }
        }
    }
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
        }],
    );
    assert_sparse_data_eq(&t, vec![(0, data.to_vec())]);
    t
}

fn rand_ascii(seed: u64, len: usize) -> Vec<u8> {
    let mut data = vec![0; len];
    let mut rng = TestRng::seed_from_u64(seed);
    rng.fill(&mut data[..]);
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
