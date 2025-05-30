use cap_rand::distributions::Standard;
use cap_rand::Rng;

use crate::engine::bindings::wasi::random::{insecure, insecure_seed, random};
use crate::Ctx;

impl random::Host for Ctx {
    fn get_random_bytes(&mut self, len: u64) -> wasmtime::Result<Vec<u8>> {
        Ok((&mut self.random.random)
            .sample_iter(Standard)
            .take(len as usize)
            .collect())
    }

    fn get_random_u64(&mut self) -> wasmtime::Result<u64> {
        Ok(self.random.random.sample(Standard))
    }
}

impl insecure::Host for Ctx {
    fn get_insecure_random_bytes(&mut self, len: u64) -> wasmtime::Result<Vec<u8>> {
        Ok((&mut self.random.insecure_random)
            .sample_iter(Standard)
            .take(len as usize)
            .collect())
    }

    fn get_insecure_random_u64(&mut self) -> wasmtime::Result<u64> {
        Ok(self.random.insecure_random.sample(Standard))
    }
}

impl insecure_seed::Host for Ctx {
    fn insecure_seed(&mut self) -> wasmtime::Result<(u64, u64)> {
        let seed: u128 = self.random.insecure_random_seed;
        Ok((seed as u64, (seed >> 64) as u64))
    }
}
