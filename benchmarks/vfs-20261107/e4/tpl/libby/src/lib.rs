pub fn hello(n: u32) -> String { if n > 1000 { panic!("too big: {n}") } format!("hello {}", file!()) }
pub struct Big<T>(pub Vec<T>);
impl<T: Clone + std::fmt::Debug> Big<T> { pub fn show(&self) -> String { format!("{:?}", self.0) } }
