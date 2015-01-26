//! Resampler implementation, used to adapt the sound samples to the
//! output sample rate.

use std::sync::{Arc, Mutex, Condvar};
use std::sync::mpsc::Receiver;
use std::thread::Thread;
use std::num::{Int, FromPrimitive};
use std::default::Default;

use spu::SampleBuffer;

use self::fifo::Fifo;
use self::worker::AsyncResampler;

mod fifo;
mod worker;

pub struct Resampler<T> {
    async:       Arc<Async<T>>,
    last_sample: T,
}

impl<T> Resampler<T>
    where T: Copy + Send + Default + Int + FromPrimitive {
    pub fn new(source: Receiver<SampleBuffer>, rate: u32) -> Resampler<T> {
        let async = Arc::new(Async::new(rate));

        let mut async_resampler = AsyncResampler::new(source,
                                                      async.clone());

        // Spawn the asynchronous resampler thread
        Thread::spawn(move || {
            async_resampler.resample();
            println!("Resampling thread done");
        });

        Resampler {
            async:      async,
            last_sample: Default::default(),
        }
    }

    pub fn fill_buf(&mut self, buf: &mut [T]) {
        let mut atomic = self.async.atomic.lock().unwrap();

        atomic.out_samples += buf.len() as u32;

        for sample in buf.iter_mut() {
            let new_sample =
                match atomic.fifo.pop() {
                    Some(s) => s,
                    // Fifo is empty, duplicate the last sample
                    None    => self.last_sample,
                };


            self.last_sample = new_sample;
            *sample          = new_sample;
        }

        // Notify the writer that we made some room in the FIFO
        self.async.stall.notify_one();
    }

    pub fn async(&self) -> Arc<Async<T>> {
        self.async.clone()
    }
}

/// Part of the `ASync` state that must be accessed atomically
struct Atomic<T> {
    fifo:         Fifo<T>,
    /// Number of samples requested by the backend since the last
    /// adjustment (even missed samples in case of empty FIFO
    /// count!). Used to compute the average output sample rate.
    out_samples:  u32,
    /// Sampling ratio: emulator sampling rate / target sampling
    /// rate. Will be adjusted at runtime based on the computed
    /// average sample rate
    ratio:        f32,
    /// Resampling ratio estimation state machine
    training:      Training,
}

/// State shared between the main thread, the SDL2 callback and the
/// resampling worker thread.
pub struct Async<T> {
    /// Atomic access substructure
    atomic: Mutex<Atomic<T>>,
    /// Convar used when waiting on the atomic FIFO
    stall:  Condvar,
}

impl<T> Async<T>
    where T: Copy + Send + Default + Int + FromPrimitive {
    fn new(rate: u32) -> Async<T> {
        // Initial educated guess for the sampling ratio. This is just
        // used while starting up, it'll be replaced by the measured
        // ratio soon enough.
        let ratio = ::spu::SAMPLE_RATE as f32 / rate as f32;

        let atomic =
            Atomic {
                fifo:         Fifo::new(),
                out_samples:  0,
                ratio:        ratio,
                training:     Training::Init(2),
            };

        Async {
            atomic: Mutex::new(atomic),
            stall:  Condvar::new(),
        }
    }

    /// Adjust the resampling ratio. `in_samples` is the number of
    /// samples generated by the emulator since the last adjustment
    /// (counting even the samples potentially dropped in case of a
    /// FIFO overflow). This call resets the internal sample counter.
    pub fn adjust_resampling(&self, in_samples: u32) {
        let mut atomic = self.atomic.lock().unwrap();

        let out_samples = atomic.out_samples;

        // Reset the atomic counter
        atomic.out_samples = 0;

        // Compute the resampling factor since the last adjust
        let r = in_samples as f32 / out_samples as f32;

        match atomic.training {
            Training::Init(c) => {
                // We're just starting, ignore the current measure and
                // wait for the next
                let c = c - 1;

                atomic.training =
                    if c > 0 {
                        Training::Init(c)
                    } else {
                        Training::Measure
                    }
            }
            Training::Measure => {
                // Our first real sample, let's use this value directly
                info!("Measured sound sample rate: {}Hz",
                      (::spu::SAMPLE_RATE as f32 / r));

                atomic.ratio = r;
                atomic.training = Training::Adjust;
            }
            Training::Adjust => {
                // Compute the delta with the current ratio
                let delta = r - atomic.ratio;

                // Avoid brutal sample rate changes by dividing the
                // delta. That way sudden spikes will be smoothed out
                // and long term tendencies will settle in
                // progressively
                let delta = delta / RATIO_AVERAGE_SMOOTH;

                // Update the ratio
                atomic.ratio += delta;
            }
        }
    }
}

/// Sample Rate training
#[derive(Copy)]
enum Training {
    /// We're starting, the measured sample rate is probably not
    /// trustworthy
    Init(u8),
    /// We're taking the next mesured sample rate as our estimate
    Measure,
    /// We're adjusting the measured sample rate incrementally
    Adjust,
}

/// Constant used to smooth resampling ratio adjustments
const RATIO_AVERAGE_SMOOTH: f32 = 10.;
