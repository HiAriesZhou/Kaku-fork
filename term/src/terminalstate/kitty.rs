use crate::terminalstate::image::*;
use crate::terminalstate::{ImageAttachParams, PlacementInfo};
use crate::{StableRowIndex, TerminalState};
use ::image::{
    DynamicImage, GenericImage, GenericImageView, ImageBuffer, RgbImage, Rgba, RgbaImage,
};
use anyhow::Context;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use wezterm_cell::image::ImageDataType;
use wezterm_escape_parser::apc::{
    KittyFrameCompositionMode, KittyImage, KittyImageCompression, KittyImageData, KittyImageDelete,
    KittyImageFormat, KittyImageFrame, KittyImageFrameCompose, KittyImagePlacement,
    KittyImageTransmit, KittyImageVerbosity,
};
use wezterm_surface::change::ImageData;

#[derive(Debug, Default)]
pub struct KittyImageState {
    accumulator: Vec<KittyImage>,
    max_image_id: u32,
    number_to_id: HashMap<u32, u32>,
    id_to_data: HashMap<u32, Arc<ImageData>>,
    placements: HashMap<(u32, Option<u32>), PlacementInfo>,
    virtual_placements: HashMap<(u32, Option<u32>), KittyImagePlacement>,
    used_memory: usize,
}

impl KittyImageState {
    fn remove_data_for_id(&mut self, image_id: u32) {
        if let Some(data) = self.id_to_data.remove(&image_id) {
            self.used_memory = self.used_memory.saturating_sub(data.len());
        }
    }

    fn record_id_to_data(&mut self, image_id: u32, data: Arc<ImageData>) {
        if image_id != 0 {
            self.remove_data_for_id(image_id);
        }
        self.prune_unreferenced();
        self.used_memory += data.len();
        self.id_to_data.insert(image_id, data);
    }

    fn prune_unreferenced(&mut self) {
        // Reduced from 320MB to 128MB to lower memory footprint.
        // TODO: make this configurable via TerminalConfiguration
        let budget = 128 * 1024 * 1024;
        if self.used_memory > budget {
            let referenced: HashSet<u32> = self
                .placements
                .keys()
                .chain(self.virtual_placements.keys())
                .map(|(k, _)| *k)
                .collect();
            let target = self.used_memory - budget;
            let mut freed = 0;
            self.id_to_data.retain(|id, data| {
                if referenced.contains(id) || freed > target {
                    true
                } else {
                    freed += data.len();
                    false
                }
            });

            log::info!(
                "using {} RAM for images, pruned {}",
                self.used_memory,
                freed
            );
            self.used_memory = self.used_memory.saturating_sub(freed);
        }
    }
}

impl TerminalState {
    fn kitty_img_place(
        &mut self,
        image_id: Option<u32>,
        image_number: Option<u32>,
        placement: KittyImagePlacement,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        let image_id = match image_id {
            Some(id) => id,
            None => *self
                .kitty_img
                .number_to_id
                .get(
                    &image_number
                        .ok_or_else(|| anyhow::anyhow!("no image_id or image_number specified!"))?,
                )
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "image_number has no matching image id {:?} in number_to_id",
                        image_number
                    )
                })?,
        };

        log::trace!(
            "kitty_img_place image_id {:?} image_no {:?} placement {:?} verb {:?}",
            image_id,
            image_number,
            placement,
            verbosity
        );
        if image_id != 0 {
            self.kitty_remove_placement(image_id, placement.placement_id);
        }
        let img = Arc::clone(self.kitty_img.id_to_data.get(&image_id).ok_or_else(|| {
            anyhow::anyhow!(
                "no matching image id {} in id_to_data for image_number {:?}",
                image_id,
                image_number
            )
        })?);

        if placement.virtual_placement {
            anyhow::ensure!(
                placement.columns != Some(0) && placement.rows != Some(0),
                "zero virtual placement dimensions"
            );
            self.kitty_img
                .virtual_placements
                .insert((image_id, placement.placement_id), placement);
            return Ok(());
        }

        let (image_width, image_height) = img.data().dimensions()?;

        let info = self.assign_image_to_cells(ImageAttachParams {
            image_width,
            image_height,
            source_width: placement.w,
            source_height: placement.h,
            source_origin_x: placement.x.unwrap_or(0),
            source_origin_y: placement.y.unwrap_or(0),
            cell_padding_left: placement.x_offset.unwrap_or(0) as u16,
            cell_padding_top: placement.y_offset.unwrap_or(0) as u16,
            data: img,
            style: ImageAttachStyle::Kitty,
            z_index: placement.z_index.unwrap_or(0),
            columns: placement.columns.map(|x| x as usize),
            rows: placement.rows.map(|x| x as usize),
            image_id: Some(image_id),
            placement_id: placement.placement_id,
            do_not_move_cursor: placement.do_not_move_cursor,
        })?;

        self.kitty_img
            .placements
            .insert((image_id, placement.placement_id), info);
        log::trace!(
            "record placement for {} (image_number {:?}) {:?}",
            image_id,
            image_number,
            placement.placement_id
        );

        Ok(())
    }

    fn kitty_img_inner(&mut self, img: KittyImage) -> anyhow::Result<()> {
        match self
            .coalesce_kitty_accumulation(img)
            .context("coalesce_kitty_accumulation")?
        {
            KittyImage::TransmitData {
                transmit,
                verbosity,
            } => {
                self.kitty_img_transmit(transmit, verbosity)?;
                Ok(())
            }
            KittyImage::TransmitDataAndDisplay {
                transmit,
                placement,
                verbosity,
            } => {
                log::trace!("TransmitDataAndDisplay {:#?} {:#?}", transmit, placement);
                let image_number = transmit.image_number;
                let image_id = self.kitty_img_transmit(transmit, verbosity)?;
                self.kitty_img_place(Some(image_id), image_number, placement, verbosity)
            }
            _ => anyhow::bail!("impossible KittImage variant"),
        }
    }

    pub(crate) fn kitty_img(&mut self, img: KittyImage) -> anyhow::Result<()> {
        log::trace!("{:?}", img);
        if !self.config.enable_kitty_graphics() {
            return Ok(());
        }
        // Intermediate chunks do not change any displayed image. Avoid scanning
        // scrollback for each 4 KiB chunk of a large upload.
        if !matches!(&img,
            KittyImage::TransmitData { transmit, .. }
            | KittyImage::TransmitDataAndDisplay { transmit, .. }
            if transmit.more_data_follows)
        {
            self.dirty_kitty_placeholder_lines();
        }
        let verbosity = img.verbosity();
        match img {
            KittyImage::Query { transmit } => match transmit.data.load_data() {
                Ok(_) => {
                    self.kitty_send_response(
                        verbosity,
                        true,
                        transmit.image_id,
                        transmit.image_number,
                        "OK".to_string(),
                    );
                }
                Err(err) => {
                    self.kitty_send_response(
                        verbosity,
                        false,
                        transmit.image_id,
                        transmit.image_number,
                        format!("ERROR:{:#}", err),
                    );
                }
            },
            KittyImage::TransmitData {
                transmit,
                verbosity,
            } => {
                let more_data_follows = transmit.more_data_follows;
                let img = KittyImage::TransmitData {
                    transmit,
                    verbosity,
                };
                if more_data_follows {
                    self.kitty_img.accumulator.push(img);
                } else {
                    self.kitty_img_inner(img)?;
                }
            }
            KittyImage::TransmitDataAndDisplay {
                transmit,
                placement,
                verbosity,
            } => {
                let more_data_follows = transmit.more_data_follows;
                let img = KittyImage::TransmitDataAndDisplay {
                    transmit,
                    placement,
                    verbosity,
                };
                if more_data_follows {
                    self.kitty_img.accumulator.push(img);
                } else {
                    self.kitty_img_inner(img)?;
                }
            }
            KittyImage::Display {
                image_id,
                image_number,
                placement,
                verbosity,
            } => {
                self.kitty_img_place(image_id, image_number, placement, verbosity)?;
            }
            KittyImage::Delete {
                what:
                    KittyImageDelete::ByImageId {
                        image_id,
                        placement_id,
                        delete,
                    },
                verbosity,
            } => {
                log::trace!(
                    "remove a placement: image_id {} placement_id {:?} delete {} verb {:?}",
                    image_id,
                    placement_id,
                    delete,
                    verbosity
                );

                self.kitty_remove_placement(image_id, placement_id);

                if delete
                    && !self
                        .kitty_img
                        .placements
                        .keys()
                        .chain(self.kitty_img.virtual_placements.keys())
                        .any(|(id, _)| *id == image_id)
                {
                    self.kitty_img.remove_data_for_id(image_id);
                }
            }
            KittyImage::Delete {
                what: KittyImageDelete::All { delete },
                verbosity: _,
            } => {
                self.kitty_remove_all_placements(delete);
            }
            KittyImage::Delete { what, verbosity } => {
                log::warn!("unhandled KittyImage::Delete {:?} {:?}", what, verbosity);
            }
            KittyImage::TransmitFrame {
                transmit,
                frame,
                verbosity,
            } => {
                if let Err(err) = self.kitty_frame_transmit(transmit, frame, verbosity) {
                    log::error!("Error {:#} while handling KittyImage::TransmitFrame", err,);
                }
            }
            KittyImage::ComposeFrame { frame, verbosity } => {
                if let Err(err) = self.kitty_frame_compose(frame, verbosity) {
                    log::error!("Error {:#} while handling KittyImage::ComposeFrame", err);
                }
            }
        };

        Ok(())
    }

    fn kitty_remove_placement_from_model(
        &mut self,
        image_id: u32,
        placement_id: Option<u32>,
        info: PlacementInfo,
    ) {
        let seqno = self.seqno;
        let screen = self.screen_mut();
        let range =
            screen.stable_range(&(info.first_row..info.first_row + info.rows as StableRowIndex));
        for idx in range {
            let line = screen.line_mut(idx);
            for c in line.cells_mut() {
                c.attrs_mut()
                    .detach_image_with_placement(image_id, placement_id);
            }
            line.update_last_change_seqno(seqno);
        }
    }

    fn kitty_remove_placement(&mut self, image_id: u32, placement_id: Option<u32>) {
        self.kitty_img
            .virtual_placements
            .retain(|(id, p), _| *id != image_id || (placement_id.is_some() && *p != placement_id));
        if placement_id.is_some() {
            if let Some(info) = self.kitty_img.placements.remove(&(image_id, placement_id)) {
                log::trace!("removed placement {} {:?}", image_id, placement_id);
                self.kitty_remove_placement_from_model(image_id, placement_id, info);
            }
        } else {
            let mut to_clear = vec![];
            for (id, p) in self.kitty_img.placements.keys() {
                if *id == image_id {
                    to_clear.push(*p);
                }
            }
            for p in to_clear.into_iter() {
                if let Some(info) = self.kitty_img.placements.remove(&(image_id, p)) {
                    self.kitty_remove_placement_from_model(image_id, p, info);
                }
            }
        }

        log::trace!(
            "after remove: there are {} placements, {} images, {} memory",
            self.kitty_img.placements.len(),
            self.kitty_img.id_to_data.len(),
            self.kitty_img.used_memory,
        );
    }

    pub(crate) fn kitty_remove_all_placements(&mut self, delete: bool) {
        for ((image_id, p), info) in std::mem::take(&mut self.kitty_img.placements).into_iter() {
            self.kitty_remove_placement_from_model(image_id, p, info);
        }
        if delete {
            let virtual_placements = &self.kitty_img.virtual_placements;
            self.kitty_img.id_to_data.retain(|id, _| {
                virtual_placements
                    .keys()
                    .any(|(virtual_id, _)| virtual_id == id)
            });
            self.kitty_img.used_memory = self
                .kitty_img
                .id_to_data
                .values()
                .map(|data| data.len())
                .sum();
            let id_to_data = &self.kitty_img.id_to_data;
            self.kitty_img
                .number_to_id
                .retain(|_, id| id_to_data.contains_key(id));
        }
    }

    fn kitty_send_response(
        &mut self,
        verbosity: KittyImageVerbosity,
        success: bool,
        image_id: Option<u32>,
        image_no: Option<u32>,
        message: String,
    ) {
        match verbosity {
            KittyImageVerbosity::Verbose => {}
            KittyImageVerbosity::OnlyErrors => {
                if success {
                    return;
                }
            }
            KittyImageVerbosity::Quiet => {
                return;
            }
        }

        log::trace!("Query Response: {}", message);

        match (image_id, image_no) {
            (Some(id), Some(no)) => {
                self.write_fmt_to_pty(
                    "kitty image response with image_id and image_no",
                    format_args!("\x1b_GI={},i={};{}\x1b\\", no, id, message),
                );
            }
            (Some(id), None) => {
                self.write_fmt_to_pty(
                    "kitty image response with image_id",
                    format_args!("\x1b_Gi={};{}\x1b\\", id, message),
                );
            }
            (None, Some(no)) => {
                self.write_fmt_to_pty(
                    "kitty image response with image_no",
                    format_args!("\x1b_GI={};{}\x1b\\", no, message),
                );
            }
            (None, None) => {
                self.write_fmt_to_pty(
                    "kitty image response",
                    format_args!("\x1b_G{}\x1b\\", message),
                );
            }
        }
        self.flush_pty("kitty image response");
    }

    fn kitty_frame_compose(
        &mut self,
        frame: KittyImageFrameCompose,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        let image_id = match frame.image_number {
            Some(no) => match self.kitty_img.number_to_id.get(&no) {
                Some(id) => *id,
                None => {
                    self.kitty_send_response(
                        verbosity,
                        false,
                        frame.image_id,
                        frame.image_number,
                        "ENOENT".to_string(),
                    );
                    anyhow::bail!("no such image_number {}", no);
                }
            },
            None => frame.image_id.ok_or_else(|| {
                self.kitty_send_response(
                    verbosity,
                    false,
                    frame.image_id,
                    frame.image_number,
                    "ENOENT".to_string(),
                );
                anyhow::anyhow!("no image_id")
            })?,
        };

        let src_frame = frame.source_frame.ok_or_else(|| {
            self.kitty_send_response(
                verbosity,
                false,
                frame.image_id,
                frame.image_number,
                "ENOENT".to_string(),
            );
            anyhow::anyhow!("missing source frame")
        })? as usize;
        let target_frame = frame.target_frame.ok_or_else(|| {
            self.kitty_send_response(
                verbosity,
                false,
                frame.image_id,
                frame.image_number,
                "ENOENT".to_string(),
            );
            anyhow::anyhow!("missing target frame")
        })? as usize;

        let img = self
            .kitty_img
            .id_to_data
            .get(&image_id)
            .ok_or_else(|| anyhow::anyhow!("invalid image id {}", image_id))?;

        let mut img = img.data();
        match &mut *img {
            ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                anyhow::bail!("invalid image type")
            }
            ImageDataType::Rgba8 {
                width,
                height,
                data,
                hash,
            } => {
                anyhow::ensure!(
                    src_frame == target_frame && src_frame == 1,
                    "src_frame={} target_frame={} but there is only a single frame",
                    src_frame,
                    target_frame
                );

                let src = clip_view(
                    *width,
                    *height,
                    data.as_mut_slice(),
                    frame.src_x,
                    frame.src_y,
                    frame.w,
                    frame.h,
                )?;

                {
                    let mut dest: ImageBuffer<Rgba<u8>, &mut [u8]> =
                        ImageBuffer::from_raw(*width, *height, data.as_mut_slice())
                            .ok_or_else(|| anyhow::anyhow!("ill formed image"))?;

                    blit(
                        &mut dest,
                        &src,
                        frame.x.unwrap_or(0),
                        frame.y.unwrap_or(0),
                        frame.composition_mode,
                    )?;
                }

                *hash = ImageDataType::hash_bytes(data);
            }
            ImageDataType::AnimRgba8 {
                width,
                height,
                frames,
                hashes,
                ..
            } => {
                anyhow::ensure!(
                    src_frame > 0 && src_frame <= frames.len(),
                    "src_frame {} is out of range",
                    src_frame
                );
                anyhow::ensure!(
                    target_frame > 0 && target_frame <= frames.len(),
                    "target_frame {} is out of range",
                    target_frame
                );

                let src = clip_view(
                    *width,
                    *height,
                    frames[src_frame - 1].as_mut_slice(),
                    frame.src_x,
                    frame.src_y,
                    frame.w,
                    frame.h,
                )?;

                {
                    let mut dest: ImageBuffer<Rgba<u8>, &mut [u8]> = ImageBuffer::from_raw(
                        *width,
                        *height,
                        frames[target_frame - 1].as_mut_slice(),
                    )
                    .ok_or_else(|| anyhow::anyhow!("ill formed image"))?;

                    blit(
                        &mut dest,
                        &src,
                        frame.x.unwrap_or(0),
                        frame.y.unwrap_or(0),
                        frame.composition_mode,
                    )?;
                }
                hashes[target_frame - 1] = ImageDataType::hash_bytes(&frames[target_frame - 1]);
            }
        }

        Ok(())
    }

    fn kitty_frame_transmit(
        &mut self,
        mut transmit: KittyImageTransmit,
        frame: KittyImageFrame,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<()> {
        if let Some(no) = transmit.image_number.take() {
            match self.kitty_img.number_to_id.get(&no) {
                Some(id) => {
                    transmit.image_id.replace(*id);
                }
                None => {
                    transmit.image_number.replace(no);
                }
            }
        }

        let (image_id, image_number, img) = self.kitty_img_transmit_inner(transmit)?;

        let img = match img.decode() {
            ImageDataType::Rgba8 {
                data,
                width,
                height,
                ..
            } => RgbaImage::from_vec(width, height, data)
                .ok_or_else(|| anyhow::anyhow!("data isn't rgba8"))?,
            wat => anyhow::bail!("data isn't rgba8 {:?}", wat),
        };

        let background_pixel = frame.background_pixel.unwrap_or(0);
        let background_pixel = Rgba([
            ((background_pixel >> 24) & 0xff) as u8,
            ((background_pixel >> 16) & 0xff) as u8,
            ((background_pixel >> 8) & 0xff) as u8,
            (background_pixel & 0xff) as u8,
        ]);

        let anim = match self.kitty_img.id_to_data.get(&image_id) {
            Some(anim) => anim,
            None => {
                self.kitty_send_response(
                    verbosity,
                    false,
                    Some(image_id),
                    image_number,
                    "ENOENT".to_string(),
                );
                anyhow::bail!(
                    "no matching image id {} in id_to_data for image_number {:?}",
                    image_id,
                    image_number
                )
            }
        };

        let mut anim = anim.data();
        let x = frame.x.unwrap_or(0);
        let y = frame.y.unwrap_or(0);
        let frame_gap = Duration::from_millis(match frame.duration_ms {
            None | Some(0) => 40,
            Some(n) => n.into(),
        });

        match &mut *anim {
            ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                anyhow::bail!("Expected decoded image for image id {}", image_id)
            }
            ImageDataType::Rgba8 {
                data,
                width,
                height,
                hash,
            } => {
                let base_frame = match frame.base_frame {
                    Some(1) => Some(1),
                    None => None,
                    Some(n) => anyhow::bail!(
                        "attempted to copy frame {} but there is only a single frame",
                        n
                    ),
                };

                match frame.frame_number {
                    Some(1) => {
                        // Edit in place
                        let len = data.len();
                        {
                            let mut anim_img: ImageBuffer<Rgba<u8>, &mut [u8]> =
                                ImageBuffer::from_raw(*width, *height, data.as_mut_slice())
                                    .ok_or_else(|| {
                                        anyhow::anyhow!(
                                            "ImageBuffer::from_raw failed for single \
                                             frame of {}x{} ({} bytes)",
                                            width,
                                            height,
                                            len
                                        )
                                    })?;

                            blit(&mut anim_img, &img, x, y, frame.composition_mode)?;
                        }
                        *hash = ImageDataType::hash_bytes(data);
                    }
                    Some(2) | None => {
                        // Create a second frame

                        let mut new_frame = if base_frame.is_some() {
                            RgbaImage::from_vec(*width, *height, data.clone()).unwrap()
                        } else {
                            RgbaImage::from_pixel(*width, *height, background_pixel)
                        };

                        blit(&mut new_frame, &img, x, y, frame.composition_mode)?;

                        let new_frame_data = new_frame.into_vec();
                        let new_frame_hash = ImageDataType::hash_bytes(&new_frame_data);

                        let frames = vec![std::mem::take(data), new_frame_data];
                        let durations = vec![Duration::from_millis(0), frame_gap];
                        let hashes = vec![*hash, new_frame_hash];

                        *anim = ImageDataType::AnimRgba8 {
                            width: *width,
                            height: *height,
                            frames,
                            durations,
                            hashes,
                        };
                    }
                    Some(n) => anyhow::bail!(
                        "attempted to edit frame {} but there is only a single frame",
                        n
                    ),
                }
            }
            ImageDataType::AnimRgba8 {
                width,
                height,
                frames,
                durations,
                hashes,
            } => {
                let frame_no = frame.frame_number.unwrap_or(frames.len() as u32 + 1);
                if frame_no == frames.len() as u32 + 1 {
                    // Append a new frame

                    let mut new_frame = match frame.base_frame {
                        None => RgbaImage::from_pixel(*width, *height, background_pixel),
                        Some(n) => {
                            let n = n as usize;
                            anyhow::ensure!(
                                n > 0 && n <= frames.len(),
                                "attempted to copy frame {} which is outside range 1-{}",
                                n,
                                frames.len()
                            );
                            RgbaImage::from_vec(*width, *height, frames[n - 1].clone()).unwrap()
                        }
                    };

                    blit(&mut new_frame, &img, x, y, frame.composition_mode)?;

                    let new_frame_data = new_frame.into_vec();
                    let new_frame_hash = ImageDataType::hash_bytes(&new_frame_data);

                    frames.push(new_frame_data);
                    hashes.push(new_frame_hash);
                    durations.push(frame_gap);
                } else {
                    anyhow::ensure!(
                        frame_no > 0 && frame_no <= frames.len() as u32,
                        "attempted to edit frame {} which is outside range 1-{}",
                        frame_no,
                        frames.len()
                    );

                    let frame_no = frame_no as usize;

                    let len = frames[frame_no - 1].len();
                    {
                        let mut anim_img: ImageBuffer<Rgba<u8>, &mut [u8]> = ImageBuffer::from_raw(
                            *width,
                            *height,
                            frames[frame_no - 1].as_mut_slice(),
                        )
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "ImageBuffer::from_raw failed for single \
                                     frame of {}x{} ({} bytes)",
                                width,
                                height,
                                len
                            )
                        })?;

                        blit(&mut anim_img, &img, x, y, frame.composition_mode)?;
                    }
                    hashes[frame_no - 1] = ImageDataType::hash_bytes(&frames[frame_no - 1]);
                }
            }
        }

        Ok(())
    }

    fn kitty_img_transmit_inner(
        &mut self,
        transmit: KittyImageTransmit,
    ) -> anyhow::Result<(u32, Option<u32>, ImageDataType)> {
        log::trace!("transmit {:?}", transmit);
        let (id, no) = match (transmit.image_id, transmit.image_number) {
            (Some(_), Some(_)) => {
                // TODO: send an EINVAL error back here
                anyhow::bail!("cannot use both i= and I= in the same request");
            }
            (None, None) => {
                // Assume image id 0
                (0, None)
            }
            (Some(id), None) => (id, None),
            (None, Some(no)) => {
                let id = self.kitty_img.max_image_id + 1;
                self.kitty_img.number_to_id.insert(no, id);
                (id, Some(no))
            }
        };

        let data = transmit
            .data
            .load_data()
            .context("data should have been materialized in coalesce_kitty_accumulation")?;

        let data = match transmit.compression {
            KittyImageCompression::None => data,
            KittyImageCompression::Deflate => {
                miniz_oxide::inflate::decompress_to_vec_zlib(&data)
                    .map_err(|e| anyhow::anyhow!("decompressing data: {:?}", e))?
            }
        };

        let img = match transmit.format {
            None | Some(KittyImageFormat::Rgba) | Some(KittyImageFormat::Rgb) => {
                let (width, height) = match (transmit.width, transmit.height) {
                    (Some(w), Some(h)) => (w, h),
                    _ => {
                        anyhow::bail!("missing width/height info for kitty img");
                    }
                };

                check_image_dimensions(width, height)?;

                let data = match transmit.format {
                    Some(KittyImageFormat::Rgb) => {
                        let img = DynamicImage::ImageRgb8(
                            RgbImage::from_vec(width, height, data)
                                .ok_or_else(|| anyhow::anyhow!("failed to decode image"))?,
                        );
                        let img = img.into_rgba8();
                        img.into_vec()
                    }
                    _ => data,
                };

                anyhow::ensure!(
                    width * height * 4 == data.len() as u32,
                    "transmit data len is {} but it doesn't match width*height*4 {}x{}x4 = {}",
                    data.len(),
                    width,
                    height,
                    width * height * 4
                );

                ImageDataType::new_single_frame(width, height, data)
            }
            Some(KittyImageFormat::Png) => {
                let info = dimensions(&data)?;
                check_image_dimensions(info.width, info.height)?;
                let decoded = image::load_from_memory(&data).context("decode png")?;
                let (width, height) = decoded.dimensions();
                let data = decoded.into_rgba8().into_vec();
                ImageDataType::new_single_frame(width, height, data)
            }
        };

        Ok((id, no, img))
    }

    fn kitty_img_transmit(
        &mut self,
        transmit: KittyImageTransmit,
        verbosity: KittyImageVerbosity,
    ) -> anyhow::Result<u32> {
        let (image_id, image_number, img) = self.kitty_img_transmit_inner(transmit)?;
        self.kitty_img.max_image_id = self.kitty_img.max_image_id.max(image_id);

        let img = self
            .raw_image_to_image_data(img)
            .context("storing image data")?;
        self.kitty_img.record_id_to_data(image_id, img);

        if image_number.is_some() {
            self.kitty_send_response(
                verbosity,
                true,
                Some(image_id),
                image_number,
                "OK".to_string(),
            );
        }

        Ok(image_id)
    }

    fn coalesce_kitty_accumulation(&mut self, img: KittyImage) -> anyhow::Result<KittyImage> {
        if self.kitty_img.accumulator.is_empty() {
            Ok(img)
        } else {
            let mut data = vec![];
            let mut trans;
            let place;
            let final_verbosity = img.verbosity();

            self.kitty_img.accumulator.push(img);

            let mut empty_data = KittyImageData::Direct(String::new());
            match self.kitty_img.accumulator.remove(0) {
                KittyImage::TransmitData { transmit, .. } => {
                    trans = transmit;
                    place = None;
                    std::mem::swap(&mut empty_data, &mut trans.data);
                }
                KittyImage::TransmitDataAndDisplay {
                    transmit,
                    placement,
                    ..
                } => {
                    place = Some(placement);
                    trans = transmit;
                    std::mem::swap(&mut empty_data, &mut trans.data);
                }
                _ => unreachable!(),
            }
            data.push(empty_data);

            for item in self.kitty_img.accumulator.drain(..) {
                match item {
                    KittyImage::TransmitData { transmit, .. }
                    | KittyImage::TransmitDataAndDisplay { transmit, .. } => {
                        data.push(transmit.data);
                    }
                    _ => unreachable!(),
                }
            }

            let mut b64_decoded = vec![];
            for mut data in data.into_iter() {
                match &mut data {
                    KittyImageData::DirectBin(b) => {
                        b64_decoded.append(b);
                    }
                    KittyImageData::Direct(b) => {
                        if !b.is_empty() {
                            b64_decoded.append(&mut data.load_data()?);
                        }
                    }
                    data => {
                        anyhow::bail!("expected data chunks to be Direct data, found {:#?}", data)
                    }
                }
            }

            trans.data = KittyImageData::DirectBin(b64_decoded);

            if let Some(placement) = place {
                Ok(KittyImage::TransmitDataAndDisplay {
                    transmit: trans,
                    placement,
                    verbosity: final_verbosity,
                })
            } else {
                Ok(KittyImage::TransmitData {
                    transmit: trans,
                    verbosity: final_verbosity,
                })
            }
        }
    }
}

/// Make a copy of the source region.
/// Ideally we wouldn't need this, but Rust's mutability rules
/// make it very awkward to mutably reference a frame while
/// an immutable reference exists to a separate frame.
fn clip_view(
    width: u32,
    height: u32,
    data: &mut [u8],
    src_x: Option<u32>,
    src_y: Option<u32>,
    view_width: Option<u32>,
    view_height: Option<u32>,
) -> anyhow::Result<RgbaImage> {
    let src = ImageBuffer::from_raw(width, height, data)
        .ok_or_else(|| anyhow::anyhow!("ill formed image"))?;

    let src_x = src_x.unwrap_or(0);
    let src_y = src_y.unwrap_or(0);

    let view_width = view_width.unwrap_or(width);
    let view_height = view_height.unwrap_or(height);

    let (view_width, view_height) =
        image::imageops::overlay_bounds((width, height), (view_width, view_height), src_x, src_y);

    let view = src.view(src_x, src_y, view_width, view_height);

    let mut tmp = RgbaImage::new(view_width, view_height);
    tmp.copy_from(&*view, 0, 0).context("copy source image")?;
    Ok(tmp)
}

fn blit<D, S, P>(
    dest: &mut D,
    src: &S,
    x: u32,
    y: u32,
    mode: KittyFrameCompositionMode,
) -> anyhow::Result<()>
where
    D: GenericImage<Pixel = P>,
    S: GenericImageView<Pixel = P>,
{
    match mode {
        KittyFrameCompositionMode::Overwrite => {
            ::image::imageops::replace(dest, src, x.into(), y.into());
        }
        KittyFrameCompositionMode::AlphaBlending => {
            ::image::imageops::overlay(dest, src, x.into(), y.into());
        }
    }
    Ok(())
}

impl TerminalState {
    pub(crate) fn append_kitty_placeholder_diacritics(&mut self, text: &str) -> bool {
        if !self.config.enable_kitty_graphics()
            || !text
                .chars()
                .all(|c| KITTY_DIACRITICS.binary_search(&(c as u32)).is_ok())
        {
            return false;
        }
        let x = if self.wrap_next {
            self.cursor.x
        } else if let Some(x) = self.cursor.x.checked_sub(1) {
            x
        } else {
            return false;
        };
        let y = self.cursor.y;
        let Some(cell) = self.screen_mut().get_cell(x, y).cloned() else {
            return false;
        };
        if !cell.str().starts_with('\u{10eeee}') {
            return false;
        }
        // PTY reads may split a placeholder from its protocol diacritics.
        let grapheme = format!("{}{text}", cell.str());
        let seqno = self.seqno;
        self.screen_mut()
            .set_cell_grapheme(x, y, &grapheme, 1, cell.attrs().clone(), seqno);
        true
    }

    fn dirty_kitty_placeholder_lines(&mut self) {
        let seqno = self.seqno;
        for screen in [&mut self.screen.screen, &mut self.screen.alt_screen] {
            screen.for_each_phys_line_mut(|_, line| {
                if has_kitty_placeholder(line) {
                    line.update_last_change_seqno(seqno);
                }
            });
        }
    }

    /// Resolve copies only: the screen retains the text for editing, scrolling
    /// and subsequent image replacement. Ordinary lines keep the borrowed path.
    pub fn kitty_placeholder_lines(
        &self,
        rows: std::ops::Range<StableRowIndex>,
    ) -> Option<(StableRowIndex, Vec<wezterm_surface::Line>)> {
        if !self.config.enable_kitty_graphics() {
            return None;
        }
        let screen = if self.is_primary_peek() {
            self.primary_screen()
        } else {
            self.screen()
        };
        let range = screen.stable_range(&rows);
        let first = screen.phys_to_stable_row_index(range.start);
        let mut result = None;
        screen.with_phys_lines(range, |lines| {
            if lines.iter().any(|line| has_kitty_placeholder(line)) {
                result = Some((
                    first,
                    lines
                        .iter()
                        .map(|line| self.resolve_kitty_placeholder_line(line))
                        .collect(),
                ));
            }
        });
        result
    }

    pub(crate) fn resolve_kitty_placeholder_line(
        &self,
        line: &wezterm_surface::Line,
    ) -> wezterm_surface::Line {
        use wezterm_cell::{Cell, CellAttributes};
        let mut resolved = line.clone();
        let mut previous = None;
        for cell in line.visible_cells() {
            let mut chars = cell.str().chars();
            if chars.next() != Some('\u{10eeee}') {
                previous = None;
                continue;
            }
            let fg = cell.attrs().foreground();
            let underline = cell.attrs().underline_color();
            let mut values = chars.filter_map(|c| {
                KITTY_DIACRITICS
                    .binary_search(&(c as u32))
                    .ok()
                    .map(|v| v as u32)
            });
            let row = values.next();
            let col = values.next();
            let high = values.next();
            let mut r = row.unwrap_or(0);
            let mut c = col.unwrap_or(0);
            let mut h = high.unwrap_or(0);
            if let Some((prev_x, prev_fg, prev_underline, prev_r, prev_c, prev_h)) = previous {
                if cell.cell_index() == prev_x + 1 && fg == prev_fg && underline == prev_underline {
                    if row.is_none() {
                        r = prev_r;
                    }
                    if col.is_none() && r == prev_r {
                        c = prev_c + 1;
                    }
                    if high.is_none() && r == prev_r && c == prev_c + 1 {
                        h = prev_h;
                    }
                }
            }
            previous = Some((cell.cell_index(), fg, underline, r, c, h));
            // Text attributes other than the background are protocol metadata,
            // not glyph decorations. Missing images must still be blank.
            let mut attrs = CellAttributes::default();
            attrs.set_background(cell.attrs().background());
            if h <= 255 {
                let id = kitty_color_id(fg) | (h << 24);
                let placement_id = kitty_color_id(underline);
                let placement = self
                    .kitty_img
                    .virtual_placements
                    .iter()
                    .filter(|((image_id, p), _)| {
                        *image_id == id && (placement_id == 0 || p.unwrap_or(0) == placement_id)
                    })
                    .min_by_key(|((_, p), _)| p.unwrap_or(0));
                if let Some(((id, p), placement)) = placement {
                    if let Some(image) = self.kitty_placeholder_image(*id, *p, placement, r, c) {
                        attrs.set_image(image);
                    }
                }
            }
            resolved.set_cell(cell.cell_index(), Cell::new(' ', attrs), self.seqno);
        }
        resolved
    }

    fn kitty_placeholder_image(
        &self,
        id: u32,
        placement_id: Option<u32>,
        placement: &KittyImagePlacement,
        row: u32,
        col: u32,
    ) -> Option<wezterm_cell::image::ImageCell> {
        use wezterm_cell::image::{ImageCell, TextureCoordinate};
        let image = self.kitty_img.id_to_data.get(&id)?;
        let (width, height) = image.data().dimensions().ok()?;
        let x = placement.x.unwrap_or(0).min(width);
        let y = placement.y.unwrap_or(0).min(height);
        let w = placement.w.unwrap_or(width - x).min(width - x) as f64;
        let h = placement.h.unwrap_or(height - y).min(height - y) as f64;
        let cw = (self.pixel_width / self.screen().physical_cols).max(1) as f64;
        let ch = (self.pixel_height / self.screen().physical_rows).max(1) as f64;
        if w == 0.0 || h == 0.0 {
            return None;
        }
        let cols = placement.columns.map(f64::from);
        let rows = placement.rows.map(f64::from);
        let scale = match (cols, rows) {
            (Some(c), Some(r)) => (c * cw / w).min(r * ch / h),
            (Some(c), None) => c * cw / w,
            (None, Some(r)) => r * ch / h,
            (None, None) => 1.0,
        };
        if scale <= 0.0 {
            return None;
        }
        let dx = f64::from(col) * cw;
        let dy = f64::from(row) * ch;
        let dw = w * scale;
        let dh = h * scale;
        if dx >= dw || dy >= dh {
            return None;
        }
        let right = (dx + cw).min(dw);
        let bottom = (dy + ch).min(dh);
        Some(ImageCell::with_z_index(
            TextureCoordinate::new_f32(
                ((f64::from(x) + dx / scale) / f64::from(width)) as f32,
                ((f64::from(y) + dy / scale) / f64::from(height)) as f32,
            ),
            TextureCoordinate::new_f32(
                ((f64::from(x) + right / scale) / f64::from(width)) as f32,
                ((f64::from(y) + bottom / scale) / f64::from(height)) as f32,
            ),
            Arc::clone(image),
            placement.z_index.unwrap_or(0),
            (
                0,
                0,
                (cw - (right - dx)).round() as u16,
                (ch - (bottom - dy)).round() as u16,
            ),
            Some(id),
            placement_id,
        ))
    }
}

fn has_kitty_placeholder(line: &wezterm_surface::Line) -> bool {
    line.visible_cells()
        .any(|cell| cell.str().starts_with('\u{10eeee}'))
}

fn kitty_color_id(color: wezterm_cell::color::ColorAttribute) -> u32 {
    use wezterm_cell::color::ColorAttribute;
    match color {
        ColorAttribute::PaletteIndex(index) => u32::from(index),
        ColorAttribute::TrueColorWithDefaultFallback(rgb)
        | ColorAttribute::TrueColorWithPaletteFallback(rgb, _) => {
            let (r, g, b, _) = rgb.to_srgb_u8();
            (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
        }
        ColorAttribute::Default => 0,
    }
}

// Kitty's rowcolumn-diacritics.txt, in protocol index order (Unicode 6.0).
// https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders
const KITTY_DIACRITICS: &[u32] = &[
    0x305, 0x30d, 0x30e, 0x310, 0x312, 0x33d, 0x33e, 0x33f, 0x346, 0x34a, 0x34b, 0x34c, 0x350,
    0x351, 0x352, 0x357, 0x35b, 0x363, 0x364, 0x365, 0x366, 0x367, 0x368, 0x369, 0x36a, 0x36b,
    0x36c, 0x36d, 0x36e, 0x36f, 0x483, 0x484, 0x485, 0x486, 0x487, 0x592, 0x593, 0x594, 0x595,
    0x597, 0x598, 0x599, 0x59c, 0x59d, 0x59e, 0x59f, 0x5a0, 0x5a1, 0x5a8, 0x5a9, 0x5ab, 0x5ac,
    0x5af, 0x5c4, 0x610, 0x611, 0x612, 0x613, 0x614, 0x615, 0x616, 0x617, 0x657, 0x658, 0x659,
    0x65a, 0x65b, 0x65d, 0x65e, 0x6d6, 0x6d7, 0x6d8, 0x6d9, 0x6da, 0x6db, 0x6dc, 0x6df, 0x6e0,
    0x6e1, 0x6e2, 0x6e4, 0x6e7, 0x6e8, 0x6eb, 0x6ec, 0x730, 0x732, 0x733, 0x735, 0x736, 0x73a,
    0x73d, 0x73f, 0x740, 0x741, 0x743, 0x745, 0x747, 0x749, 0x74a, 0x7eb, 0x7ec, 0x7ed, 0x7ee,
    0x7ef, 0x7f0, 0x7f1, 0x7f3, 0x816, 0x817, 0x818, 0x819, 0x81b, 0x81c, 0x81d, 0x81e, 0x81f,
    0x820, 0x821, 0x822, 0x823, 0x825, 0x826, 0x827, 0x829, 0x82a, 0x82b, 0x82c, 0x82d, 0x951,
    0x953, 0x954, 0xf82, 0xf83, 0xf86, 0xf87, 0x135d, 0x135e, 0x135f, 0x17dd, 0x193a, 0x1a17,
    0x1a75, 0x1a76, 0x1a77, 0x1a78, 0x1a79, 0x1a7a, 0x1a7b, 0x1a7c, 0x1b6b, 0x1b6d, 0x1b6e, 0x1b6f,
    0x1b70, 0x1b71, 0x1b72, 0x1b73, 0x1cd0, 0x1cd1, 0x1cd2, 0x1cda, 0x1cdb, 0x1ce0, 0x1dc0, 0x1dc1,
    0x1dc3, 0x1dc4, 0x1dc5, 0x1dc6, 0x1dc7, 0x1dc8, 0x1dc9, 0x1dcb, 0x1dcc, 0x1dd1, 0x1dd2, 0x1dd3,
    0x1dd4, 0x1dd5, 0x1dd6, 0x1dd7, 0x1dd8, 0x1dd9, 0x1dda, 0x1ddb, 0x1ddc, 0x1ddd, 0x1dde, 0x1ddf,
    0x1de0, 0x1de1, 0x1de2, 0x1de3, 0x1de4, 0x1de5, 0x1de6, 0x1dfe, 0x20d0, 0x20d1, 0x20d4, 0x20d5,
    0x20d6, 0x20d7, 0x20db, 0x20dc, 0x20e1, 0x20e7, 0x20e9, 0x20f0, 0x2cef, 0x2cf0, 0x2cf1, 0x2de0,
    0x2de1, 0x2de2, 0x2de3, 0x2de4, 0x2de5, 0x2de6, 0x2de7, 0x2de8, 0x2de9, 0x2dea, 0x2deb, 0x2dec,
    0x2ded, 0x2dee, 0x2def, 0x2df0, 0x2df1, 0x2df2, 0x2df3, 0x2df4, 0x2df5, 0x2df6, 0x2df7, 0x2df8,
    0x2df9, 0x2dfa, 0x2dfb, 0x2dfc, 0x2dfd, 0x2dfe, 0x2dff, 0xa66f, 0xa67c, 0xa67d, 0xa6f0, 0xa6f1,
    0xa8e0, 0xa8e1, 0xa8e2, 0xa8e3, 0xa8e4, 0xa8e5, 0xa8e6, 0xa8e7, 0xa8e8, 0xa8e9, 0xa8ea, 0xa8eb,
    0xa8ec, 0xa8ed, 0xa8ee, 0xa8ef, 0xa8f0, 0xa8f1, 0xaab0, 0xaab2, 0xaab3, 0xaab7, 0xaab8, 0xaabe,
    0xaabf, 0xaac1, 0xfe20, 0xfe21, 0xfe22, 0xfe23, 0xfe24, 0xfe25, 0xfe26, 0x10a0f, 0x10a38,
    0x1d185, 0x1d186, 0x1d187, 0x1d188, 0x1d189, 0x1d1aa, 0x1d1ab, 0x1d1ac, 0x1d1ad, 0x1d242,
    0x1d243, 0x1d244,
];
