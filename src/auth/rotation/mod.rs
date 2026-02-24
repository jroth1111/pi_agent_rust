//! Token rotation policies and strategies.
//!
//! This module provides configurable rotation policies for OAuth tokens,
//! including time-based, usage-based, and entropy-based rotation triggers.

mod entropy;
mod policy;

pub use entropy::{EntropyCalculator, UsageSample};
pub use policy::{
    EffectiveRotationConfig, ProviderRotationConfig, RotationPolicy, RotationReason,
    RotationStrategy,
};
