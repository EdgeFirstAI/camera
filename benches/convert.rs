use criterion::{criterion_group, criterion_main, Criterion};
use edgefirst_camera::image::{self, Image, ImageManager};

pub fn benchmark_resize(c: &mut Criterion) {
    let fmts = [image::RGBA, image::RGB3, image::YUYV, image::NV12];
    let dims = [
        (320, 240),
        (640, 480),
        (960, 540),
        (1920, 1080),
        (3840, 2160),
    ];
    let mgr = ImageManager::new().unwrap();

    for src_fmt in fmts.iter() {
        let mut group = c.benchmark_group(format!("resize/{}", src_fmt));
        for src_dim in dims.iter() {
            for dst_dim in dims.iter() {
                let src = Image::new(src_dim.0, src_dim.1, *src_fmt).unwrap();
                let dst = Image::new(dst_dim.0, dst_dim.1, image::RGBA).unwrap();
                group.bench_with_input(
                    format!("{}x{}-{}x{}", src_dim.0, src_dim.1, dst_dim.0, dst_dim.1),
                    &(src, dst),
                    |b, imgs| b.iter(|| mgr.convert(&imgs.0, &imgs.1, None)),
                );
            }
        }
    }
}

criterion_group!(benches, benchmark_resize);
criterion_main!(benches);
