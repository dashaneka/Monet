//! Hardware decoding needs no setup: GStreamer registers hardware decoders
//! (VA-API, NVDEC, V4L2, ...) only for codecs the machine's driver supports,
//! and ranks them above software ones. This only handles the fallback when
//! one of them fails at runtime.

use gst::prelude::*;

fn is_hardware_decoder(factory: &gst::ElementFactory) -> bool {
    let klass = factory.klass();
    klass.contains("Decoder") && klass.contains("Hardware")
}

pub fn hardware_in_use(pipeline: &gst::Element) -> bool {
    let Some(bin) = pipeline.downcast_ref::<gst::Bin>() else {
        return false;
    };
    bin.iterate_recurse()
        .into_iter()
        .flatten()
        .any(|element| element.factory().is_some_and(|f| is_hardware_decoder(&f)))
}

/// Stops hardware decoders from being picked for the rest of this process.
/// Returns false if they were already off, so a failure can't retry forever.
pub fn disable_hardware() -> bool {
    let mut changed = false;
    for feature in gst::Registry::get().features(gst::ElementFactory::static_type()) {
        if let Ok(factory) = feature.downcast::<gst::ElementFactory>()
            && is_hardware_decoder(&factory)
            && factory.rank() > gst::Rank::NONE
        {
            factory.set_rank(gst::Rank::NONE);
            changed = true;
        }
    }
    changed
}
