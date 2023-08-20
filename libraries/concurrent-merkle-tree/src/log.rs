macro_rules! solana_logging {
    ($message:literal, $($arg:tt)*) => {
        #[cfg(feature = "log")]
        ::solomka_program::msg!($message, $($arg)*);
    };
    ($message:literal) => {
        #[cfg(feature = "log")]
        ::solomka_program::msg!($message);
    };
}

macro_rules! log_compute {
    () => {
        #[cfg(all(feature = "sol-log", feature = "log"))]
        ::solomka_program::log::sol_log_compute_units();
    };
}
