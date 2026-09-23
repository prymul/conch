//! `conch-difftest`: the differential test harness that runs the same
//! shell script through conch and a real oracle shell (bash, and where
//! relevant `sh`) and compares the result.
//!
//! See `README.md` (in this crate's directory) for the full design
//! rationale, corpus format, and -- most importantly if you're picking
//! this crate up once `conch` is a real, working shell -- how to switch
//! the differential tests on for real.

pub mod case;
pub mod compare;
pub mod corpus;
pub mod invoke;
pub mod normalize;
pub mod report;
pub mod runner;
