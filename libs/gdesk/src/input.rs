pub const MOD_LCTRL: u16 = 1 << 0;
pub const MOD_RCTRL: u16 = 1 << 1;
pub const MOD_LSHIFT: u16 = 1 << 2;
pub const MOD_RSHIFT: u16 = 1 << 3;
pub const MOD_LALT: u16 = 1 << 4;
pub const MOD_RALT: u16 = 1 << 5;
pub const MOD_LMETA: u16 = 1 << 6;
pub const MOD_RMETA: u16 = 1 << 7;
pub const MOD_ALL: u16 = 0xFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputVerdict {
    Reject,
    Accept { release: u16 },
}

/// Lease 0 is accepted only while this guard has not enabled a session lease.
#[derive(Debug, Clone, Copy, Default)]
pub struct InputGuard {
    lease: u32,
    pressed: u16,
}

impl InputGuard {
    pub fn lease(&self) -> u32 {
        self.lease
    }

    pub fn pressed(&self) -> u16 {
        self.pressed
    }

    /// Returns the modifiers that must be released because the old lease holder may still hold them.
    pub fn rotate_lease(&mut self, lease: u32) -> u16 {
        self.lease = lease;
        std::mem::take(&mut self.pressed)
    }

    pub fn check(
        &mut self,
        lease: u32,
        client_mask: u16,
        event: Option<(u16, bool)>,
    ) -> InputVerdict {
        if lease == 0 {
            return if self.lease == 0 {
                InputVerdict::Accept { release: 0 }
            } else {
                InputVerdict::Reject
            };
        }
        if self.lease == 0 {
            self.lease = lease;
        } else if lease != self.lease {
            return InputVerdict::Reject;
        }
        let down_bit = event.filter(|(_, down)| *down).map_or(0, |(bit, _)| bit);
        let release = self.pressed & !client_mask & !down_bit;
        self.pressed &= !release;
        if let Some((bit, down)) = event {
            if down {
                self.pressed |= bit & MOD_ALL;
            } else {
                self.pressed &= !bit;
            }
        }
        InputVerdict::Accept { release }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_down_tolerates_mask_lag_and_enabled_lease_rejects_zero() {
        let mut guard = InputGuard::default();
        assert_eq!(guard.check(0, 0, None), InputVerdict::Accept { release: 0 });
        guard.rotate_lease(9);
        assert_eq!(guard.check(0, 0, None), InputVerdict::Reject);
        for _ in 0..2 {
            assert_eq!(
                guard.check(9, 0, Some((MOD_LCTRL, true))),
                InputVerdict::Accept { release: 0 }
            );
            assert_eq!(guard.pressed(), MOD_LCTRL);
        }
        assert_eq!(
            guard.check(9, MOD_LSHIFT, Some((MOD_LSHIFT, true))),
            InputVerdict::Accept { release: MOD_LCTRL }
        );
        assert_eq!(guard.pressed(), MOD_LSHIFT);
        assert_eq!(
            guard.check(9, 0, None),
            InputVerdict::Accept {
                release: MOD_LSHIFT
            }
        );
    }

    #[test]
    fn modifier_bitmap_releases_missing_keys_and_stale_lease_is_rejected() {
        let mut guard = InputGuard::default();
        assert_eq!(
            guard.check(7, MOD_LCTRL, Some((MOD_LCTRL, true))),
            InputVerdict::Accept { release: 0 }
        );
        assert_eq!(guard.check(6, MOD_LCTRL, None), InputVerdict::Reject);
        assert_eq!(guard.pressed(), MOD_LCTRL);
        assert_eq!(
            guard.check(7, 0, None),
            InputVerdict::Accept { release: MOD_LCTRL }
        );
        assert_eq!(guard.pressed(), 0);
        assert_eq!(guard.check(0, 0, None), InputVerdict::Reject);
    }
}
