//! Code shared across more than one AMBE mode -- currently just [`fec`], the Golay(23,12)/
//! Hamming(15,11) FEC codes D-STAR's and AMBE+2 half-rate's frame layer both depend on (they share
//! a bit-for-bit identical FEC/whitening/interleave layer -- see `super::dstar`'s and
//! `super::ambe_plus_2`'s own doc comments) and TIA-102.BABA uses its own copy of independently.
pub mod fec;
