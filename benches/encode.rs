use camera::{
    image,
    image::{encode_jpeg, Image},
};
use criterion::{criterion_group, criterion_main, Criterion};

fn benchmark_jpeg(img: &Image) {
    let dma = img.dmabuf();
    let mem = dma.memory_map().unwrap();
    let _ = mem.read(encode_jpeg, Some(img)).unwrap();
}

pub fn benchmark_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("jpeg");
    for dim in [
        (320, 240),
        (640, 480),
        (960, 540),
        (1280, 720),
        (1920, 1080),
        (3840, 2160),
    ]
    .iter()
    {
        let img = Image::new(dim.0, dim.1, image::RGBA).unwrap();
        group.bench_with_input(format!("{}x{}", dim.0, dim.1), &img, |b, img| {
            b.iter(|| benchmark_jpeg(img))
        });
    }
}

criterion_group!(benches, benchmark_encode);
criterion_main!(benches);
