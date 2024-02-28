use std::{
    fmt::{Debug, Formatter},
    marker::PhantomData,
    sync::atomic::{AtomicU64, Ordering},
};

pub trait Handle {
    fn from_raw(raw_handle: RawHandle) -> Self;
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct RawHandle {
    generator_id: u64,
    handle_id: u64,
}

impl RawHandle {
    pub fn generator_id(&self) -> u64 {
        self.generator_id
    }

    pub fn handle_id(&self) -> u64 {
        self.handle_id
    }
}

pub struct HandleGenerator<H: Handle> {
    /// This ID is used to bind the handles to the generator instance, i.e. it's possible to
    /// distinguish handles generated by different generators. We hope that this might
    /// prevent bugs when the library user is dealing with handles from different sources.
    generator_id: u64,
    next_handle_id: u64,
    _h: PhantomData<H>,
}

impl<H: Handle> Debug for HandleGenerator<H> {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        f.debug_struct("HandleGenerator")
            .field("generator_id", &self.generator_id)
            .field("next_handle_id", &self.next_handle_id)
            .finish_non_exhaustive()
    }
}

impl<H: Handle> HandleGenerator<H> {
    pub fn generate(&mut self) -> H {
        let handle_id = self.next_handle_id;
        self.next_handle_id = self.next_handle_id.wrapping_add(1);

        H::from_raw(RawHandle {
            generator_id: self.generator_id,
            handle_id,
        })
    }
}

pub struct HandleGeneratorGenerator<H: Handle> {
    next_handle_generator_id: AtomicU64,
    _h: PhantomData<H>,
}

impl<H: Handle> HandleGeneratorGenerator<H> {
    pub const fn new() -> Self {
        Self {
            next_handle_generator_id: AtomicU64::new(0),
            _h: PhantomData,
        }
    }

    pub fn generate(&self) -> HandleGenerator<H> {
        // There is no synchronization required and we only care about each thread seeing a
        // unique value.
        let generator_id = self
            .next_handle_generator_id
            .fetch_add(1, Ordering::Relaxed);

        HandleGenerator {
            generator_id,
            next_handle_id: 0,
            _h: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Handle, HandleGeneratorGenerator, RawHandle};

    struct TestHandle(RawHandle);

    impl Handle for TestHandle {
        fn from_raw(raw_handle: RawHandle) -> Self {
            Self(raw_handle)
        }
    }

    #[test]
    fn generated_handles_have_expected_ids() {
        let gen_gen = HandleGeneratorGenerator::<TestHandle>::new();

        for expected_generator_id in 0..100 {
            let mut gen = gen_gen.generate();

            for expected_handle_id in 0..100 {
                let handle = gen.generate();

                assert_eq!(expected_generator_id, handle.0.generator_id);
                assert_eq!(expected_handle_id, handle.0.handle_id);
            }
        }
    }
}
