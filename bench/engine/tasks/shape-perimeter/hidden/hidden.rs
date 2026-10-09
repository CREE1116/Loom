use bench_task::shapes::{Circle, Rect, Shape};
#[test] fn rect() { assert_eq!(Rect { w: 2.0, h: 3.0 }.perimeter(), 10.0); }
#[test] fn circle() { assert!((Circle { r: 1.0 }.perimeter() - 2.0 * std::f64::consts::PI).abs() < 1e-9); }
#[test] fn dyn_shape() { let s: Box<dyn Shape> = Box::new(Rect { w: 1.0, h: 1.0 }); assert_eq!(s.perimeter(), 4.0); }
