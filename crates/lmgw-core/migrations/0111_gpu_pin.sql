-- A model's pin (device-ledger design §8, ruling 8, WP5): the cards it may
-- run on, as a JSON list of the cards' stable ids (`GpuMemory.id`: NVML's
-- `GPU-<uuid>`, an AMD card's PCI address). NULL: lmgw chooses the card. One
-- card: the model runs there or not at all; several: lmgw chooses among
-- them. Nothing is rewritten: run arguments that name a card are read as a
-- pin where they stand (§8.1).
ALTER TABLE local_models ADD COLUMN gpu_pin TEXT;
ALTER TABLE aux_models ADD COLUMN gpu_pin TEXT;
ALTER TABLE audio_models ADD COLUMN gpu_pin TEXT;
ALTER TABLE image_models ADD COLUMN gpu_pin TEXT;
