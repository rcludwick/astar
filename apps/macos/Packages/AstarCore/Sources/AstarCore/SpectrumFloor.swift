// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

import Foundation

/// The noise-floor estimate the mic-analyzer canvas draws, factored out of the
/// view so it can be tested.
///
/// The characterizer picks notches by taking the **median** of its scan band and
/// keeping every bin that stands `peakMarginDb` above it. Applying that rule to
/// the *display* bins gives the operator a line to aim the slider at — but it is
/// an **estimate of the detector's threshold, not the threshold**. The display
/// spectrum is a 2048-point FFT max-folded into log bins and peak-held; the
/// detector runs a finer FFT and medians over linear frequency. Max-folding
/// keeps the loudest linear bin in each log bin, so this median — and the line
/// drawn from it — can sit several dB high, and a peak just under the line can
/// still be notched. If a notch you expected to disappear stays, raise the
/// margin further.
///
/// The band matches the detector's scan band (100–3800 Hz), not the whole
/// displayed axis (100–3900), so at least the two look at the same spectrum. It
/// is given as *bin index fractions* (0…1) rather than frequencies: the caller
/// maps Hz to a fraction with the axis's own log mapping, which lives in the app
/// target, and this stays pure arithmetic over the bins.
public enum SpectrumFloor {
    /// Median of the bins whose index fraction lies within `[lowFraction,
    /// highFraction]`. `nil` when fewer than three bins qualify (too few to call
    /// a floor) or any bin in the band is non-finite.
    public static func scanBandMedian(
        _ bins: [Float], lowFraction: Double, highFraction: Double
    ) -> Float? {
        guard bins.count > 1, lowFraction <= highFraction else { return nil }
        let last = Double(bins.count - 1)
        var band: [Float] = []
        band.reserveCapacity(bins.count)
        for (i, v) in bins.enumerated() {
            let frac = Double(i) / last
            guard frac >= lowFraction, frac <= highFraction else { continue }
            guard v.isFinite else { return nil }
            band.append(v)
        }
        guard band.count >= 3 else { return nil }
        band.sort()
        let mid = band.count / 2
        // Even counts average the two middle samples; odd counts take the middle.
        return band.count % 2 == 1 ? band[mid] : (band[mid - 1] + band[mid]) / 2
    }

    /// The dBFS value of the drawn line: the scan-band median plus the operator's
    /// margin — an estimate of the detector's threshold, see the type doc. `nil`
    /// whenever the median is (no bins yet, or a bad band).
    public static func line(
        bins: [Float], lowFraction: Double, highFraction: Double, marginDb: Double
    ) -> Float? {
        guard
            let median = scanBandMedian(
                bins, lowFraction: lowFraction, highFraction: highFraction)
        else { return nil }
        return median + Float(marginDb)
    }
}
