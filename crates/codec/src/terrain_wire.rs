use super::*;
use torse_types::{TerrainPage, TerrainReset, TerrainUnavailable, TerrainUpdate};

impl Wire for (SectionPos, Digest) {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.0.write(e)?;
        self.1.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok((Wire::read(d)?, Wire::read(d)?))
    }
}
impl Wire for (SectionPos, TerrainUnavailable) {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.0.write(e)?;
        self.1.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok((Wire::read(d)?, Wire::read(d)?))
    }
}

impl Wire for TerrainReset {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.sections.write(e)?;
        self.retained.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            sections: Wire::read(d)?,
            retained: Wire::read(d)?,
        })
    }
}
impl Wire for TerrainUnavailable {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        (match self {
            Self::Pending => 0u8,
            Self::Missing => 1,
            Self::Corrupt => 2,
            Self::Budget => 3,
        })
        .write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        match u8::read(d)? {
            0 => Ok(Self::Pending),
            1 => Ok(Self::Missing),
            2 => Ok(Self::Corrupt),
            3 => Ok(Self::Budget),
            _ => Err(Fault::invalid_data("unknown terrain availability")),
        }
    }
}
impl<S: Wire> Wire for TerrainPage<S> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.at.write(e)?;
        self.digest.write(e)?;
        self.terrain.write(e)?;
        self.light.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            at: Wire::read(d)?,
            digest: Wire::read(d)?,
            terrain: Wire::read(d)?,
            light: Wire::read(d)?,
        })
    }
}

impl Wire for torse_types::TerrainLight {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.pos.write(e)?;
        self.terrain.write(e)?;
        self.generation.write(e)?;
        self.rules.write(e)?;
        self.complete.write(e)?;
        self.emitted.write(e)?;
        self.sky.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            pos: Wire::read(d)?,
            terrain: Wire::read(d)?,
            generation: Wire::read(d)?,
            rules: Wire::read(d)?,
            complete: Wire::read(d)?,
            emitted: Wire::read(d)?,
            sky: Wire::read(d)?,
        })
    }
}
impl Wire for torse_types::TerrainLightUpdate {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.binding.write(e)?;
        self.interest_generation.write(e)?;
        self.seq.write(e)?;
        self.pages.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            binding: Wire::read(d)?,
            interest_generation: Wire::read(d)?,
            seq: Wire::read(d)?,
            pages: Wire::read(d)?,
        })
    }
}
impl<S: Wire> Wire for TerrainUpdate<S> {
    fn write(&self, e: &mut Encoder<'_>) -> EncodeResult<()> {
        self.binding.write(e)?;
        self.generation.write(e)?;
        self.seq.write(e)?;
        self.through.write(e)?;
        self.reset.write(e)?;
        self.pages.write(e)?;
        self.unavailable.write(e)
    }
    fn read(d: &mut Decoder<'_>) -> EncodeResult<Self> {
        Ok(Self {
            binding: Wire::read(d)?,
            generation: Wire::read(d)?,
            seq: Wire::read(d)?,
            through: Wire::read(d)?,
            reset: Wire::read(d)?,
            pages: Wire::read(d)?,
            unavailable: Wire::read(d)?,
        })
    }
}
