use rand::{self, Rng};

pub fn random_floats() -> impl Iterator<Item = f32> {
    let mut rng = rand::thread_rng();
    std::iter::repeat_with(move || rng.gen::<f32>() - 0.5)
}

pub fn random_vector<Element: std::iter::FromIterator<f32>>(dim: usize) -> Element {
    random_floats().take(dim).collect()
}
