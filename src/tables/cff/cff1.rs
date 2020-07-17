// Useful links:
// http://wwwimages.adobe.com/content/dam/Adobe/en/devnet/font/pdfs/5176.CFF.pdf
// http://wwwimages.adobe.com/content/dam/Adobe/en/devnet/font/pdfs/5177.Type2.pdf
// https://github.com/opentypejs/opentype.js/blob/master/src/tables/cff.js

use core::convert::TryFrom;
use core::ops::Range;

use crate::{GlyphId, OutlineBuilder, Rect, BBox};
use crate::parser::{Stream, LazyArray16, Fixed, NumFrom, TryNumFrom};
use super::{Builder, IsEven, CFFError, StringId, f32_abs, calc_subroutine_bias, conv_subroutine_index};
use super::argstack::ArgumentsStack;
use super::charset::{Charset, parse_charset};
use super::dict::DictionaryParser;
use super::index::{Index, parse_index, skip_index};

// Limits according to the Adobe Technical Note #5176, chapter 4 DICT Data.
const MAX_OPERANDS_LEN: usize = 48;

// Limits according to the Adobe Technical Note #5177 Appendix B.
const STACK_LIMIT: u8 = 10;
const MAX_ARGUMENTS_STACK_LEN: usize = 48;

const TWO_BYTE_OPERATOR_MARK: u8 = 12;

/// Enumerates some operators defined in the Adobe Technical Note #5177.
mod operator {
    pub const HORIZONTAL_STEM: u8           = 1;
    pub const VERTICAL_STEM: u8             = 3;
    pub const VERTICAL_MOVE_TO: u8          = 4;
    pub const LINE_TO: u8                   = 5;
    pub const HORIZONTAL_LINE_TO: u8        = 6;
    pub const VERTICAL_LINE_TO: u8          = 7;
    pub const CURVE_TO: u8                  = 8;
    pub const CALL_LOCAL_SUBROUTINE: u8     = 10;
    pub const RETURN: u8                    = 11;
    pub const ENDCHAR: u8                   = 14;
    pub const HORIZONTAL_STEM_HINT_MASK: u8 = 18;
    pub const HINT_MASK: u8                 = 19;
    pub const COUNTER_MASK: u8              = 20;
    pub const MOVE_TO: u8                   = 21;
    pub const HORIZONTAL_MOVE_TO: u8        = 22;
    pub const VERTICAL_STEM_HINT_MASK: u8   = 23;
    pub const CURVE_LINE: u8                = 24;
    pub const LINE_CURVE: u8                = 25;
    pub const VV_CURVE_TO: u8               = 26;
    pub const HH_CURVE_TO: u8               = 27;
    pub const SHORT_INT: u8                 = 28;
    pub const CALL_GLOBAL_SUBROUTINE: u8    = 29;
    pub const VH_CURVE_TO: u8               = 30;
    pub const HV_CURVE_TO: u8               = 31;
    pub const HFLEX: u8                     = 34;
    pub const FLEX: u8                      = 35;
    pub const HFLEX1: u8                    = 36;
    pub const FLEX1: u8                     = 37;
    pub const FIXED_16_16: u8               = 255;
}

/// Enumerates some operators defined in the Adobe Technical Note #5176,
/// Table 9 Top DICT Operator Entries
mod top_dict_operator {
    pub const CHARSET_OFFSET: u16               = 15;
    pub const CHAR_STRINGS_OFFSET: u16          = 17;
    pub const PRIVATE_DICT_SIZE_AND_OFFSET: u16 = 18;
    pub const ROS: u16                          = 1230;
    pub const FD_ARRAY: u16                     = 1236;
    pub const FD_SELECT: u16                    = 1237;
}

/// Enumerates some operators defined in the Adobe Technical Note #5176,
/// Table 23 Private DICT Operators
mod private_dict_operator {
    pub const LOCAL_SUBROUTINES_OFFSET: u16 = 19;
}

/// Enumerates Charset IDs defined in the Adobe Technical Note #5176, Table 22
mod charset_id {
    pub const ISO_ADOBE: usize = 0;
    pub const EXPERT: usize = 1;
    pub const EXPERT_SUBSET: usize = 2;
}


#[derive(Clone, Copy, Debug)]
pub struct Metadata<'a> {
    // The whole CFF table.
    // Used to resolve a local subroutine in a CID font.
    table_data: &'a [u8],

    global_subrs: Index<'a>,
    charset: Charset<'a>,
    char_strings: Index<'a>,
    kind: FontKind<'a>,
}

#[derive(Clone, Copy, Debug)]
pub enum FontKind<'a> {
    SID(SIDMetadata<'a>),
    CID(CIDMetadata<'a>),
}

#[derive(Clone, Copy, Default, Debug)]
pub struct SIDMetadata<'a> {
    local_subrs: Index<'a>,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct CIDMetadata<'a> {
    fd_array: Index<'a>,
    fd_select: FDSelect<'a>,
}

pub(crate) fn parse_metadata(data: &[u8]) -> Option<Metadata> {
    let mut s = Stream::new(data);

    // Parse Header.
    let major: u8 = s.read()?;
    s.skip::<u8>(); // minor
    let header_size: u8 = s.read()?;
    s.skip::<u8>(); // Absolute offset

    if major != 1 {
        return None;
    }

    // Jump to Name INDEX. It's not necessarily right after the header.
    if header_size > 4 {
        s.advance(usize::from(header_size) - 4);
    }

    // Skip Name INDEX.
    skip_index::<u16>(&mut s)?;

    let top_dict = parse_top_dict(&mut s)?;

    // Must be set, otherwise there are nothing to parse.
    if top_dict.char_strings_offset == 0 {
        return None;
    }

    // Skip String INDEX.
    skip_index::<u16>(&mut s)?;

    // Parse Global Subroutines INDEX.
    let global_subrs = parse_index::<u16>(&mut s)?;

    let char_strings = {
        let mut s = Stream::new_at(data, top_dict.char_strings_offset)?;
        parse_index::<u16>(&mut s)?
    };

    if char_strings.len() == 0 {
        return None;
    }

    // 'The number of glyphs is the value of the count field in the CharStrings INDEX.'
    let number_of_glyphs = u16::try_from(char_strings.len()).ok()?;

    let charset = match top_dict.charset_offset {
        Some(charset_id::ISO_ADOBE) => Charset::ISOAdobe,
        Some(charset_id::EXPERT) => Charset::Expert,
        Some(charset_id::EXPERT_SUBSET) => Charset::ExpertSubset,
        Some(offset) => parse_charset(number_of_glyphs, &mut Stream::new_at(data, offset)?)?,
        None => Charset::ISOAdobe, // default
    };

    let kind = if top_dict.has_ros {
        parse_cid_metadata(data, top_dict, number_of_glyphs)?
    } else {
        parse_sid_metadata(data, top_dict)?
    };

    Some(Metadata {
        table_data: data,
        global_subrs,
        charset,
        char_strings,
        kind,
    })
}

fn parse_sid_metadata(data: &[u8], top_dict: TopDict) -> Option<FontKind> {
    let subroutines_offset = if let Some(range) = top_dict.private_dict_range.clone() {
        parse_private_dict(data.get(range)?)
    } else {
        None
    };

    // Parse Global Subroutines INDEX.
    let mut metadata = SIDMetadata::default();

    match (top_dict.private_dict_range, subroutines_offset) {
        (Some(private_dict_range), Some(subroutines_offset)) => {
            // 'The local subroutines offset is relative to the beginning
            // of the Private DICT data.'
            if let Some(start) = private_dict_range.start.checked_add(subroutines_offset) {
                let data = data.get(start..data.len())?;
                let mut s = Stream::new(data);
                metadata.local_subrs = parse_index::<u16>(&mut s)?;
            }
        }
        _ => {}
    }

    Some(FontKind::SID(metadata))
}

fn parse_cid_metadata(data: &[u8], top_dict: TopDict, number_of_glyphs: u16) -> Option<FontKind> {
    let (charset_offset, fd_array_offset, fd_select_offset) =
        match (top_dict.charset_offset, top_dict.fd_array_offset, top_dict.fd_select_offset) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return None, // charset, FDArray and FDSelect must be set.
        };

    if charset_offset <= charset_id::EXPERT_SUBSET {
        // 'There are no predefined charsets for CID fonts.'
        // Adobe Technical Note #5176, chapter 18 CID-keyed Fonts
        return None;
    }

    let mut metadata = CIDMetadata::default();

    metadata.fd_array = {
        let mut s = Stream::new_at(data, fd_array_offset)?;
        parse_index::<u16>(&mut s)?
    };

    metadata.fd_select = {
        let mut s = Stream::new_at(data, fd_select_offset)?;
        parse_fd_select(number_of_glyphs, &mut s)?
    };

    Some(FontKind::CID(metadata))
}

#[derive(Default)]
struct TopDict {
    charset_offset: Option<usize>,
    char_strings_offset: usize,
    private_dict_range: Option<Range<usize>>,
    has_ros: bool,
    fd_array_offset: Option<usize>,
    fd_select_offset: Option<usize>,
}

fn parse_top_dict(s: &mut Stream) -> Option<TopDict> {
    let mut top_dict = TopDict::default();

    let index = parse_index::<u16>(s)?;

    // The Top DICT INDEX should have only one dictionary.
    let data = index.get(0)?;

    let mut operands_buffer = [0; MAX_OPERANDS_LEN];
    let mut dict_parser = DictionaryParser::new(data, &mut operands_buffer);
    while let Some(operator) = dict_parser.parse_next() {
        match operator.get() {
            top_dict_operator::CHARSET_OFFSET => {
                top_dict.charset_offset = dict_parser.parse_offset();
            }
            top_dict_operator::CHAR_STRINGS_OFFSET => {
                top_dict.char_strings_offset = dict_parser.parse_offset()?;
            }
            top_dict_operator::PRIVATE_DICT_SIZE_AND_OFFSET => {
                top_dict.private_dict_range = dict_parser.parse_range();
            }
            top_dict_operator::ROS => {
                top_dict.has_ros = true;
            }
            top_dict_operator::FD_ARRAY => {
                top_dict.fd_array_offset = dict_parser.parse_offset();
            }
            top_dict_operator::FD_SELECT => {
                top_dict.fd_select_offset = dict_parser.parse_offset();
            }
            _ => {}
        }
    }

    Some(top_dict)
}

fn parse_private_dict(data: &[u8]) -> Option<usize> {
    let mut operands_buffer = [0; MAX_OPERANDS_LEN];
    let mut dict_parser = DictionaryParser::new(data, &mut operands_buffer);
    while let Some(operator) = dict_parser.parse_next() {
        if operator.get() == private_dict_operator::LOCAL_SUBROUTINES_OFFSET {
            return dict_parser.parse_offset();
        }
    }

    None
}

fn parse_font_dict(data: &[u8]) -> Option<Range<usize>> {
    let mut operands_buffer = [0; MAX_OPERANDS_LEN];
    let mut dict_parser = DictionaryParser::new(data, &mut operands_buffer);
    while let Some(operator) = dict_parser.parse_next() {
        if operator.get() == top_dict_operator::PRIVATE_DICT_SIZE_AND_OFFSET {
            return dict_parser.parse_range();
        }
    }

    None
}

/// In CID fonts, to get local subroutines we have to:
///   1. Find Font DICT index via FDSelect by GID.
///   2. Get Font DICT data from FDArray using this index.
///   3. Get a Private DICT offset from a Font DICT.
///   4. Get a local subroutine offset from Private DICT.
///   5. Parse a local subroutine at offset.
fn parse_cid_local_subrs<'a>(
    data: &'a [u8],
    glyph_id: GlyphId,
    cid: &CIDMetadata,
) -> Option<Index<'a>> {
    let font_dict_index = cid.fd_select.font_dict_index(glyph_id)?;
    let font_dict_data = cid.fd_array.get(u32::from(font_dict_index))?;
    let private_dict_range = parse_font_dict(font_dict_data)?;
    let private_dict_data = data.get(private_dict_range.clone())?;
    let subroutines_offset = parse_private_dict(private_dict_data)?;

    // 'The local subroutines offset is relative to the beginning
    // of the Private DICT data.'
    let start = private_dict_range.start.checked_add(subroutines_offset)?;
    let subrs_data = data.get(start..)?;
    let mut s = Stream::new(subrs_data);
    parse_index::<u16>(&mut s)
}

pub fn outline(
    metadata: &Metadata,
    glyph_id: GlyphId,
    builder: &mut dyn OutlineBuilder,
) -> Option<Rect> {
    let data = metadata.char_strings.get(u32::from(glyph_id.0))?;
    parse_char_string(data, metadata, glyph_id, builder).ok()
}

struct CharStringParserContext<'a> {
    metadata: &'a Metadata<'a>,
    is_first_move_to: bool,
    has_move_to: bool,
    width_parsed: bool,
    stems_len: u32,
    has_endchar: bool,
    has_seac: bool,
    glyph_id: GlyphId, // Required to parse local subroutine in CID fonts.
    local_subrs: Option<Index<'a>>,
}

fn parse_char_string(
    data: &[u8],
    metadata: &Metadata,
    glyph_id: GlyphId,
    builder: &mut dyn OutlineBuilder,
) -> Result<Rect, CFFError> {
    let local_subrs = match metadata.kind {
        FontKind::SID(ref sid) => Some(sid.local_subrs),
        FontKind::CID(_) => None, // Will be resolved on request.
    };

    let mut ctx = CharStringParserContext {
        metadata,
        is_first_move_to: true,
        has_move_to: false,
        width_parsed: false,
        stems_len: 0,
        has_endchar: false,
        has_seac: false,
        glyph_id,
        local_subrs,
    };

    let mut inner_builder = Builder {
        builder,
        bbox: BBox::new(),
    };

    let mut stack = ArgumentsStack {
        data: &mut [0.0; MAX_ARGUMENTS_STACK_LEN], // 192B
        len: 0,
        max_len: MAX_ARGUMENTS_STACK_LEN,
    };
    let _ = _parse_char_string(&mut ctx, data, 0.0, 0.0, &mut stack, 0, &mut inner_builder)?;

    if !ctx.has_endchar {
        return Err(CFFError::MissingEndChar);
    }

    let bbox = inner_builder.bbox;

    // Check that bbox was changed.
    if bbox.is_default() {
        return Err(CFFError::ZeroBBox);
    }

    bbox.to_rect().ok_or(CFFError::BboxOverflow)
}


fn _parse_char_string(
    ctx: &mut CharStringParserContext,
    char_string: &[u8],
    mut x: f32,
    mut y: f32,
    stack: &mut ArgumentsStack,
    depth: u8,
    builder: &mut Builder,
) -> Result<(f32, f32), CFFError> {
    let mut s = Stream::new(char_string);
    while !s.at_end() {
        let op: u8 = s.read().ok_or(CFFError::ReadOutOfBounds)?;
        match op {
            0 | 2 | 9 | 13 | 15 | 16 | 17 => {
                // Reserved.
                return Err(CFFError::InvalidOperator);
            }
            operator::HORIZONTAL_STEM |
            operator::VERTICAL_STEM |
            operator::HORIZONTAL_STEM_HINT_MASK |
            operator::VERTICAL_STEM_HINT_MASK => {
                // y dy {dya dyb}* hstem
                // x dx {dxa dxb}* vstem
                // y dy {dya dyb}* hstemhm
                // x dx {dxa dxb}* vstemhm

                // If the stack length is uneven, than the first value is a `width`.
                let len = if stack.len().is_odd() && !ctx.width_parsed {
                    ctx.width_parsed = true;
                    stack.len() - 1
                } else {
                    stack.len()
                };

                ctx.stems_len += len as u32 >> 1;

                // We are ignoring the hint operators.
                stack.clear();
            }
            operator::VERTICAL_MOVE_TO => {
                // dy1

                let mut i = 0;
                if stack.len() == 2 && !ctx.width_parsed {
                    i += 1;
                    ctx.width_parsed = true;
                } else if stack.len() != 1 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                if ctx.is_first_move_to {
                    ctx.is_first_move_to = false;
                } else {
                    builder.close();
                }

                ctx.has_move_to = true;

                y += stack.at(i);
                builder.move_to(x, y);

                stack.clear();
            }
            operator::LINE_TO => {
                // {dxa dya}+

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                if stack.len().is_odd() {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                let mut i = 0;
                while i < stack.len() {
                    x += stack.at(i + 0);
                    y += stack.at(i + 1);
                    builder.line_to(x, y);
                    i += 2;
                }

                stack.clear();
            }
            operator::HORIZONTAL_LINE_TO => {
                // dx1 {dya dxb}*
                //     {dxa dyb}+

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                if stack.is_empty() {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                let mut i = 0;
                while i < stack.len() {
                    x += stack.at(i);
                    i += 1;
                    builder.line_to(x, y);

                    if i == stack.len() {
                        break;
                    }

                    y += stack.at(i);
                    i += 1;
                    builder.line_to(x, y);
                }

                stack.clear();
            }
            operator::VERTICAL_LINE_TO => {
                // dy1 {dxa dyb}*
                //     {dya dxb}+

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                if stack.is_empty() {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                let mut i = 0;
                while i < stack.len() {
                    y += stack.at(i);
                    i += 1;
                    builder.line_to(x, y);

                    if i == stack.len() {
                        break;
                    }

                    x += stack.at(i);
                    i += 1;
                    builder.line_to(x, y);
                }

                stack.clear();
            }
            operator::CURVE_TO => {
                // {dxa dya dxb dyb dxc dyc}+

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                if stack.len() % 6 != 0 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                let mut i = 0;
                while i < stack.len() {
                    let x1 = x + stack.at(i + 0);
                    let y1 = y + stack.at(i + 1);
                    let x2 = x1 + stack.at(i + 2);
                    let y2 = y1 + stack.at(i + 3);
                    x = x2 + stack.at(i + 4);
                    y = y2 + stack.at(i + 5);

                    builder.curve_to(x1, y1, x2, y2, x, y);
                    i += 6;
                }

                stack.clear();
            }
            operator::CALL_LOCAL_SUBROUTINE => {
                if stack.is_empty() {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                if depth == STACK_LIMIT {
                    return Err(CFFError::NestingLimitReached);
                }

                // Parse and remember the local subroutine for the current glyph.
                // Since it's a pretty complex task, we're doing it only when
                // a local subroutine is actually requested by the glyphs charstring.
                if ctx.local_subrs.is_none() {
                    if let FontKind::CID(ref cid) = ctx.metadata.kind {
                        ctx.local_subrs = parse_cid_local_subrs(
                            ctx.metadata.table_data, ctx.glyph_id, cid
                        );
                    }
                }

                if let Some(local_subrs) = ctx.local_subrs {
                    let subroutine_bias = calc_subroutine_bias(local_subrs.len());
                    let index = conv_subroutine_index(stack.pop(), subroutine_bias)?;
                    let char_string = local_subrs.get(index).ok_or(CFFError::InvalidSubroutineIndex)?;
                    let pos = _parse_char_string(ctx, char_string, x, y, stack, depth + 1, builder)?;
                    x = pos.0;
                    y = pos.1;
                } else {
                    return Err(CFFError::NoLocalSubroutines);
                }

                if ctx.has_endchar && !ctx.has_seac {
                    if !s.at_end() {
                        return Err(CFFError::DataAfterEndChar);
                    }

                    break;
                }
            }
            operator::RETURN => {
                break;
            }
            TWO_BYTE_OPERATOR_MARK => {
                // flex
                let op2: u8 = s.read().ok_or(CFFError::ReadOutOfBounds)?;
                match op2 {
                    operator::HFLEX => {
                        // dx1 dx2 dy2 dx3 dx4 dx5 dx6

                        if !ctx.has_move_to {
                            return Err(CFFError::MissingMoveTo);
                        }

                        if stack.len() != 7 {
                            return Err(CFFError::InvalidArgumentsStackLength);
                        }

                        let dx1 = x + stack.at(0);
                        let dy1 = y;
                        let dx2 = dx1 + stack.at(1);
                        let dy2 = dy1 + stack.at(2);
                        let dx3 = dx2 + stack.at(3);
                        let dy3 = dy2;
                        let dx4 = dx3 + stack.at(4);
                        let dy4 = dy2;
                        let dx5 = dx4 + stack.at(5);
                        let dy5 = y;
                        x = dx5 + stack.at(6);
                        builder.curve_to(dx1, dy1, dx2, dy2, dx3, dy3);
                        builder.curve_to(dx4, dy4, dx5, dy5, x, y);

                        stack.clear();
                    }
                    operator::FLEX => {
                        // dx1 dy1 dx2 dy2 dx3 dy3 dx4 dy4 dx5 dy5 dx6 dy6 fd

                        if !ctx.has_move_to {
                            return Err(CFFError::MissingMoveTo);
                        }

                        if stack.len() != 13 {
                            return Err(CFFError::InvalidArgumentsStackLength);
                        }

                        let dx1 = x + stack.at(0);
                        let dy1 = y + stack.at(1);
                        let dx2 = dx1 + stack.at(2);
                        let dy2 = dy1 + stack.at(3);
                        let dx3 = dx2 + stack.at(4);
                        let dy3 = dy2 + stack.at(5);
                        let dx4 = dx3 + stack.at(6);
                        let dy4 = dy3 + stack.at(7);
                        let dx5 = dx4 + stack.at(8);
                        let dy5 = dy4 + stack.at(9);
                        x = dx5 + stack.at(10);
                        y = dy5 + stack.at(11);
                        builder.curve_to(dx1, dy1, dx2, dy2, dx3, dy3);
                        builder.curve_to(dx4, dy4, dx5, dy5, x, y);

                        stack.clear();
                    }
                    operator::HFLEX1 => {
                        // dx1 dy1 dx2 dy2 dx3 dx4 dx5 dy5 dx6

                        if !ctx.has_move_to {
                            return Err(CFFError::MissingMoveTo);
                        }

                        if stack.len() != 9 {
                            return Err(CFFError::InvalidArgumentsStackLength);
                        }

                        let dx1 = x + stack.at(0);
                        let dy1 = y + stack.at(1);
                        let dx2 = dx1 + stack.at(2);
                        let dy2 = dy1 + stack.at(3);
                        let dx3 = dx2 + stack.at(4);
                        let dy3 = dy2;
                        let dx4 = dx3 + stack.at(5);
                        let dy4 = dy2;
                        let dx5 = dx4 + stack.at(6);
                        let dy5 = dy4 + stack.at(7);
                        x = dx5 + stack.at(8);
                        builder.curve_to(dx1, dy1, dx2, dy2, dx3, dy3);
                        builder.curve_to(dx4, dy4, dx5, dy5, x, y);

                        stack.clear();
                    }
                    operator::FLEX1 => {
                        // dx1 dy1 dx2 dy2 dx3 dy3 dx4 dy4 dx5 dy5 d6

                        if !ctx.has_move_to {
                            return Err(CFFError::MissingMoveTo);
                        }

                        if stack.len() != 11 {
                            return Err(CFFError::InvalidArgumentsStackLength);
                        }

                        let dx1 = x + stack.at(0);
                        let dy1 = y + stack.at(1);
                        let dx2 = dx1 + stack.at(2);
                        let dy2 = dy1 + stack.at(3);
                        let dx3 = dx2 + stack.at(4);
                        let dy3 = dy2 + stack.at(5);
                        let dx4 = dx3 + stack.at(6);
                        let dy4 = dy3 + stack.at(7);
                        let dx5 = dx4 + stack.at(8);
                        let dy5 = dy4 + stack.at(9);

                        if f32_abs(dx5 - x) > f32_abs(dy5 - y) {
                            x = dx5 + stack.at(10);
                        } else {
                            y = dy5 + stack.at(10);
                        }

                        builder.curve_to(dx1, dy1, dx2, dy2, dx3, dy3);
                        builder.curve_to(dx4, dy4, dx5, dy5, x, y);

                        stack.clear();
                    }
                    _ => {
                        return Err(CFFError::UnsupportedOperator);
                    }
                }
            }
            operator::ENDCHAR => {
                if stack.len() == 4 || (!ctx.width_parsed && stack.len() == 5) {
                    // Process 'seac'.
                    let accent_char = seac_code_to_glyph_id(&ctx.metadata.charset, stack.pop())
                        .ok_or(CFFError::InvalidSeacCode)?;
                    let base_char = seac_code_to_glyph_id(&ctx.metadata.charset, stack.pop())
                        .ok_or(CFFError::InvalidSeacCode)?;
                    let dy = stack.pop();
                    let dx = stack.pop();

                    if !ctx.width_parsed {
                        stack.pop();
                        ctx.width_parsed = true;
                    }

                    ctx.has_seac = true;

                    let base_char_string = ctx.metadata.char_strings.get(u32::from(base_char.0))
                        .ok_or(CFFError::InvalidSeacCode)?;
                    _parse_char_string(ctx, base_char_string, x, y, stack, depth + 1, builder)?;
                    x = dx;
                    y = dy;

                    let accent_char_string = ctx.metadata.char_strings.get(u32::from(accent_char.0))
                        .ok_or(CFFError::InvalidSeacCode)?;
                    _parse_char_string(ctx, accent_char_string, x, y, stack, depth + 1, builder)?;
                } else if stack.len() == 1 && !ctx.width_parsed {
                    stack.pop();
                    ctx.width_parsed = true;
                }

                if !ctx.is_first_move_to {
                    ctx.is_first_move_to = true;
                    builder.close();
                }

                if !s.at_end() {
                    return Err(CFFError::DataAfterEndChar);
                }

                ctx.has_endchar = true;

                break;
            }
            operator::HINT_MASK | operator::COUNTER_MASK => {
                let mut len = stack.len();

                // We are ignoring the hint operators.
                stack.clear();

                // If the stack length is uneven, than the first value is a `width`.
                if len.is_odd() && !ctx.width_parsed {
                    len -= 1;
                    ctx.width_parsed = true;
                }

                ctx.stems_len += len as u32 >> 1;

                s.advance(usize::num_from((ctx.stems_len + 7) >> 3));
            }
            operator::MOVE_TO => {
                // dx1 dy1

                let mut i = 0;
                if stack.len() == 3 && !ctx.width_parsed {
                    i += 1;
                    ctx.width_parsed = true;
                } else if stack.len() != 2 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                if ctx.is_first_move_to {
                    ctx.is_first_move_to = false;
                } else {
                    builder.close();
                }

                ctx.has_move_to = true;

                x += stack.at(i + 0);
                y += stack.at(i + 1);
                builder.move_to(x, y);

                stack.clear();
            }
            operator::HORIZONTAL_MOVE_TO => {
                // dx1

                let mut i = 0;
                if stack.len() == 2 && !ctx.width_parsed {
                    i += 1;
                    ctx.width_parsed = true;
                } else if stack.len() != 1 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                if ctx.is_first_move_to {
                    ctx.is_first_move_to = false;
                } else {
                    builder.close();
                }

                ctx.has_move_to = true;

                x += stack.at(i);
                builder.move_to(x, y);

                stack.clear();
            }
            operator::CURVE_LINE => {
                // {dxa dya dxb dyb dxc dyc}+ dxd dyd

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                if stack.len() < 8 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                if (stack.len() - 2) % 6 != 0 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                let mut i = 0;
                while i < stack.len() - 2 {
                    let x1 = x + stack.at(i + 0);
                    let y1 = y + stack.at(i + 1);
                    let x2 = x1 + stack.at(i + 2);
                    let y2 = y1 + stack.at(i + 3);
                    x = x2 + stack.at(i + 4);
                    y = y2 + stack.at(i + 5);

                    builder.curve_to(x1, y1, x2, y2, x, y);
                    i += 6;
                }

                x += stack.at(i + 0);
                y += stack.at(i + 1);
                builder.line_to(x, y);

                stack.clear();
            }
            operator::LINE_CURVE => {
                // {dxa dya}+ dxb dyb dxc dyc dxd dyd

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                if stack.len() < 8 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                if (stack.len() - 6).is_odd() {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                let mut i = 0;
                while i < stack.len() - 6 {
                    x += stack.at(i + 0);
                    y += stack.at(i + 1);

                    builder.line_to(x, y);
                    i += 2;
                }

                let x1 = x + stack.at(i + 0);
                let y1 = y + stack.at(i + 1);
                let x2 = x1 + stack.at(i + 2);
                let y2 = y1 + stack.at(i + 3);
                x = x2 + stack.at(i + 4);
                y = y2 + stack.at(i + 5);
                builder.curve_to(x1, y1, x2, y2, x, y);

                stack.clear();
            }
            operator::VV_CURVE_TO => {
                // dx1? {dya dxb dyb dyc}+

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                let mut i = 0;

                // The odd argument count indicates an X position.
                if stack.len().is_odd() {
                    x += stack.at(0);
                    i += 1;
                }

                if (stack.len() - i) % 4 != 0 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                while i < stack.len() {
                    let x1 = x;
                    let y1 = y + stack.at(i + 0);
                    let x2 = x1 + stack.at(i + 1);
                    let y2 = y1 + stack.at(i + 2);
                    x = x2;
                    y = y2 + stack.at(i + 3);

                    builder.curve_to(x1, y1, x2, y2, x, y);
                    i += 4;
                }

                stack.clear();
            }
            operator::HH_CURVE_TO => {
                // dy1? {dxa dxb dyb dxc}+

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                let mut i = 0;

                // The odd argument count indicates an Y position.
                if stack.len().is_odd() {
                    y += stack.at(0);
                    i += 1;
                }

                if (stack.len() - i) % 4 != 0 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                while i < stack.len() {
                    let x1 = x + stack.at(i + 0);
                    let y1 = y;
                    let x2 = x1 + stack.at(i + 1);
                    let y2 = y1 + stack.at(i + 2);
                    x = x2 + stack.at(i + 3);
                    y = y2;

                    builder.curve_to(x1, y1, x2, y2, x, y);
                    i += 4;
                }

                stack.clear();
            }
            operator::SHORT_INT => {
                let n = s.read::<i16>().ok_or(CFFError::ReadOutOfBounds)?;
                stack.push(f32::from(n))?;
            }
            operator::CALL_GLOBAL_SUBROUTINE => {
                if stack.is_empty() {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                if depth == STACK_LIMIT {
                    return Err(CFFError::NestingLimitReached);
                }

                let subroutine_bias = calc_subroutine_bias(ctx.metadata.global_subrs.len());
                let index = conv_subroutine_index(stack.pop(), subroutine_bias)?;
                let char_string = ctx.metadata.global_subrs.get(index)
                    .ok_or(CFFError::InvalidSubroutineIndex)?;
                let pos = _parse_char_string(ctx, char_string, x, y, stack, depth + 1, builder)?;
                x = pos.0;
                y = pos.1;

                if ctx.has_endchar && !ctx.has_seac {
                    if !s.at_end() {
                        return Err(CFFError::DataAfterEndChar);
                    }

                    break;
                }
            }
            operator::VH_CURVE_TO => {
                // dy1 dx2 dy2 dx3 {dxa dxb dyb dyc dyd dxe dye dxf}* dyf?
                //                 {dya dxb dyb dxc dxd dxe dye dyf}+ dxf?

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                if stack.len() < 4 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                stack.reverse();
                while !stack.is_empty() {
                    if stack.len() < 4 {
                        return Err(CFFError::InvalidArgumentsStackLength);
                    }

                    let x1 = x;
                    let y1 = y + stack.pop();
                    let x2 = x1 + stack.pop();
                    let y2 = y1 + stack.pop();
                    x = x2 + stack.pop();
                    y = y2 + if stack.len() == 1 { stack.pop() } else { 0.0 };
                    builder.curve_to(x1, y1, x2, y2, x, y);
                    if stack.is_empty() {
                        break;
                    }

                    if stack.len() < 4 {
                        return Err(CFFError::InvalidArgumentsStackLength);
                    }

                    let x1 = x + stack.pop();
                    let y1 = y;
                    let x2 = x1 + stack.pop();
                    let y2 = y1 + stack.pop();
                    y = y2 + stack.pop();
                    x = x2 + if stack.len() == 1 { stack.pop() } else { 0.0 };
                    builder.curve_to(x1, y1, x2, y2, x, y);
                }

                debug_assert!(stack.is_empty());
            }
            operator::HV_CURVE_TO => {
                // dx1 dx2 dy2 dy3 {dya dxb dyb dxc dxd dxe dye dyf}* dxf?
                //                 {dxa dxb dyb dyc dyd dxe dye dxf}+ dyf?

                if !ctx.has_move_to {
                    return Err(CFFError::MissingMoveTo);
                }

                if stack.len() < 4 {
                    return Err(CFFError::InvalidArgumentsStackLength);
                }

                stack.reverse();
                while !stack.is_empty() {
                    if stack.len() < 4 {
                        return Err(CFFError::InvalidArgumentsStackLength);
                    }

                    let x1 = x + stack.pop();
                    let y1 = y;
                    let x2 = x1 + stack.pop();
                    let y2 = y1 + stack.pop();
                    y = y2 + stack.pop();
                    x = x2 + if stack.len() == 1 { stack.pop() } else { 0.0 };
                    builder.curve_to(x1, y1, x2, y2, x, y);
                    if stack.is_empty() {
                        break;
                    }

                    if stack.len() < 4 {
                        return Err(CFFError::InvalidArgumentsStackLength);
                    }

                    let x1 = x;
                    let y1 = y + stack.pop();
                    let x2 = x1 + stack.pop();
                    let y2 = y1 + stack.pop();
                    x = x2 + stack.pop();
                    y = y2 + if stack.len() == 1 { stack.pop() } else { 0.0 };
                    builder.curve_to(x1, y1, x2, y2, x, y);
                }

                debug_assert!(stack.is_empty());
            }
            32..=246 => {
                let n = i16::from(op) - 139;
                stack.push(f32::from(n))?;
            }
            247..=250 => {
                let b1: u8 = s.read().ok_or(CFFError::ReadOutOfBounds)?;
                let n = (i16::from(op) - 247) * 256 + i16::from(b1) + 108;
                debug_assert!((108..=1131).contains(&n));
                stack.push(f32::from(n))?;
            }
            251..=254 => {
                let b1: u8 = s.read().ok_or(CFFError::ReadOutOfBounds)?;
                let n = -(i16::from(op) - 251) * 256 - i16::from(b1) - 108;
                debug_assert!((-1131..=-108).contains(&n));
                stack.push(f32::from(n))?;
            }
            operator::FIXED_16_16 => {
                let n = s.read::<Fixed>().ok_or(CFFError::ReadOutOfBounds)?;
                stack.push(n.0)?;
            }
        }
    }

    // TODO: 'A charstring subroutine must end with either an endchar or a return operator.'

    Ok((x, y))
}

fn seac_code_to_glyph_id(charset: &Charset, n: f32) -> Option<GlyphId> {
    let code = u8::try_num_from(n)?;

    let sid = STANDARD_ENCODING[code as usize];
    let sid = StringId(u16::from(sid));

    match charset {
        Charset::ISOAdobe => {
            // Not sure why code should be less than 228/zcaron, but this is what harfbuzz does.
            if code < 228 { Some(GlyphId(sid.0)) } else { None }
        }
        Charset::Expert | Charset::ExpertSubset => None,
        _ => charset.sid_to_gid(sid),
    }
}


#[derive(Clone, Copy, Debug)]
enum FDSelect<'a> {
    Format0(LazyArray16<'a, u8>),
    Format3(&'a [u8]), // It's easier to parse it in-place.
}

impl Default for FDSelect<'_> {
    fn default() -> Self {
        FDSelect::Format0(LazyArray16::default())
    }
}

impl FDSelect<'_> {
    fn font_dict_index(&self, glyph_id: GlyphId) -> Option<u8> {
        match self {
            FDSelect::Format0(ref array) => array.get(glyph_id.0),
            FDSelect::Format3(ref data) => {
                let mut s = Stream::new(data);
                let number_of_ranges: u16 = s.read()?;
                if number_of_ranges == 0 {
                    return None;
                }

                // 'A sentinel GID follows the last range element and serves
                // to delimit the last range in the array.'
                // So we can simply increase the number of ranges by one.
                let number_of_ranges = number_of_ranges.checked_add(1)?;

                // Range is: GlyphId + u8
                let mut prev_first_glyph: GlyphId = s.read()?;
                let mut prev_index: u8 = s.read()?;
                for _ in 1..number_of_ranges {
                    let curr_first_glyph: GlyphId = s.read()?;
                    if (prev_first_glyph..curr_first_glyph).contains(&glyph_id) {
                        return Some(prev_index);
                    } else {
                        prev_index = s.read()?;
                    }

                    prev_first_glyph = curr_first_glyph;
                }

                None
            }
        }
    }
}

fn parse_fd_select<'a>(number_of_glyphs: u16, s: &mut Stream<'a>) -> Option<FDSelect<'a>> {
    let format: u8 = s.read()?;
    match format {
        0 => Some(FDSelect::Format0(s.read_array16(number_of_glyphs)?)),
        3 => Some(FDSelect::Format3(s.tail()?)),
        _ => None,
    }
}


/// The Standard Encoding as defined in the Adobe Technical Note #5176 Appendix B.
const STANDARD_ENCODING: [u8;256] = [
      0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,
      0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,
      1,   2,   3,   4,   5,   6,   7,   8,   9,  10,  11,  12,  13,  14,  15,  16,
     17,  18,  19,  20,  21,  22,  23,  24,  25,  26,  27,  28,  29,  30,  31,  32,
     33,  34,  35,  36,  37,  38,  39,  40,  41,  42,  43,  44,  45,  46,  47,  48,
     49,  50,  51,  52,  53,  54,  55,  56,  57,  58,  59,  60,  61,  62,  63,  64,
     65,  66,  67,  68,  69,  70,  71,  72,  73,  74,  75,  76,  77,  78,  79,  80,
     81,  82,  83,  84,  85,  86,  87,  88,  89,  90,  91,  92,  93,  94,  95,   0,
      0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,
      0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,
      0,  96,  97,  98,  99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110,
      0, 111, 112, 113, 114,   0, 115, 116, 117, 118, 119, 120, 121, 122,   0, 123,
      0, 124, 125, 126, 127, 128, 129, 130, 131,   0, 132, 133,   0, 134, 135, 136,
    137,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,   0,
      0, 138,   0, 139,   0,   0,   0,   0, 140, 141, 142, 143,   0,   0,   0,   0,
      0, 144,   0,   0,   0, 145,   0,   0, 146, 147, 148, 149,   0,   0,   0,   0,
];


#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;
    use std::string::String;
    use std::fmt::Write;
    use crate::writer;
    use writer::TtfType::*;

    struct Builder(String);
    impl OutlineBuilder for Builder {
        fn move_to(&mut self, x: f32, y: f32) {
            write!(&mut self.0, "M {} {} ", x, y).unwrap();
        }

        fn line_to(&mut self, x: f32, y: f32) {
            write!(&mut self.0, "L {} {} ", x, y).unwrap();
        }

        fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
            write!(&mut self.0, "Q {} {} {} {} ", x1, y1, x, y).unwrap();
        }

        fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
            write!(&mut self.0, "C {} {} {} {} {} {} ", x1, y1, x2, y2, x, y).unwrap();
        }

        fn close(&mut self) {
            write!(&mut self.0, "Z ").unwrap();
        }
    }

    fn gen_cff(
        global_subrs: &[&[writer::TtfType]],
        local_subrs: &[&[writer::TtfType]],
        chars: &[writer::TtfType],
    ) -> Vec<u8> {
        fn gen_global_subrs(subrs: &[&[writer::TtfType]]) -> Vec<u8> {
            let mut w = writer::Writer::new();
            for v1 in subrs {
                for v2 in v1.iter() {
                    w.write(*v2);
                }
            }
            w.data
        }

        fn gen_local_subrs(subrs: &[&[writer::TtfType]]) -> Vec<u8> {
            let mut w = writer::Writer::new();
            for v1 in subrs {
                for v2 in v1.iter() {
                    w.write(*v2);
                }
            }
            w.data
        }

        const EMPTY_INDEX_SIZE: usize = 2;
        const INDEX_HEADER_SIZE: usize = 5;

        // TODO: support multiple subrs
        assert!(global_subrs.len() <= 1);
        assert!(local_subrs.len() <= 1);

        let global_subrs_data = gen_global_subrs(global_subrs);
        let local_subrs_data = gen_local_subrs(local_subrs);
        let chars_data = writer::convert(chars);

        assert!(global_subrs_data.len() < 255);
        assert!(local_subrs_data.len() < 255);
        assert!(chars_data.len() < 255);

        let mut w = writer::Writer::new();
        // Header
        w.write(UInt8(1)); // major version
        w.write(UInt8(0)); // minor version
        w.write(UInt8(4)); // header size
        w.write(UInt8(0)); // absolute offset

        // Name INDEX
        w.write(UInt16(0)); // count

        // Top DICT
        // INDEX
        w.write(UInt16(1)); // count
        w.write(UInt8(1)); // offset size
        w.write(UInt8(1)); // index[0]

        let top_dict_idx2 = if local_subrs.is_empty() { 3 } else { 6 };
        w.write(UInt8(top_dict_idx2)); // index[1]
        // Item 0
        let mut charstr_offset = w.offset() + 2;
        charstr_offset += EMPTY_INDEX_SIZE; // String INDEX

        // Global Subroutines INDEX
        if !global_subrs_data.is_empty() {
            charstr_offset += INDEX_HEADER_SIZE + global_subrs_data.len();
        } else {
            charstr_offset += EMPTY_INDEX_SIZE;
        }

        if !local_subrs_data.is_empty() {
            charstr_offset += 3;
        }

        w.write(CFFInt(charstr_offset as i32));
        w.write(UInt8(top_dict_operator::CHAR_STRINGS_OFFSET as u8));

        if !local_subrs_data.is_empty() {
            // Item 1
            w.write(CFFInt(2)); // length
            w.write(CFFInt((charstr_offset + INDEX_HEADER_SIZE + chars_data.len()) as i32)); // offset
            w.write(UInt8(top_dict_operator::PRIVATE_DICT_SIZE_AND_OFFSET as u8));
        }

        // String INDEX
        w.write(UInt16(0)); // count

        // Global Subroutines INDEX
        if global_subrs_data.is_empty() {
            w.write(UInt16(0)); // count
        } else {
            w.write(UInt16(1)); // count
            w.write(UInt8(1)); // offset size
            w.write(UInt8(1)); // index[0]
            w.write(UInt8(global_subrs_data.len() as u8 + 1)); // index[1]
            w.data.extend_from_slice(&global_subrs_data);
        }

        // CharString INDEX
        w.write(UInt16(1)); // count
        w.write(UInt8(1)); // offset size
        w.write(UInt8(1)); // index[0]
        w.write(UInt8(chars_data.len() as u8 + 1)); // index[1]
        w.data.extend_from_slice(&chars_data);

        if !local_subrs_data.is_empty() {
            // The local subroutines offset is relative to the beginning of the Private DICT data.

            // Private DICT
            w.write(CFFInt(2));
            w.write(UInt8(private_dict_operator::LOCAL_SUBROUTINES_OFFSET as u8));

            // Local Subroutines INDEX
            w.write(UInt16(1)); // count
            w.write(UInt8(1)); // offset size
            w.write(UInt8(1)); // index[0]
            w.write(UInt8(local_subrs_data.len() as u8 + 1)); // index[1]
            w.data.extend_from_slice(&local_subrs_data);
        }

        w.data
    }

    #[test]
    fn unsupported_version() {
        let data = writer::convert(&[
            UInt8(10), // major version, only 1 is supported
            UInt8(0), // minor version
            UInt8(4), // header size
            UInt8(0), // absolute offset
        ]);

        assert!(parse_metadata(&data).is_none());
    }

    #[test]
    fn non_default_header_size() {
        let data = writer::convert(&[
            // Header
            UInt8(1), // major version
            UInt8(0), // minor version
            UInt8(8), // header size
            UInt8(0), // absolute offset

            // no-op, should be skipped
            UInt8(0),
            UInt8(0),
            UInt8(0),
            UInt8(0),

            // Name INDEX
            UInt16(0), // count

            // Top DICT
            // INDEX
            UInt16(1), // count
            UInt8(1), // offset size
            UInt8(1), // index[0]
            UInt8(3), // index[1]
            // Data
            CFFInt(21),
            UInt8(top_dict_operator::CHAR_STRINGS_OFFSET as u8),

            // String INDEX
            UInt16(0), // count

            // Global Subroutines INDEX
            UInt16(0), // count

            // CharString INDEX
            UInt16(1), // count
            UInt8(1), // offset size
            UInt8(1), // index[0]
            UInt8(4), // index[1]
            // Data
            CFFInt(10),
            UInt8(operator::HORIZONTAL_MOVE_TO),
            UInt8(operator::ENDCHAR),
        ]);

        let metadata = parse_metadata(&data).unwrap();
        let mut builder = Builder(String::new());
        let char_str = metadata.char_strings.get(0).unwrap();
        let rect = parse_char_string(char_str, &metadata, GlyphId(0), &mut builder).unwrap();

        assert_eq!(builder.0, "M 10 0 Z ");
        assert_eq!(rect, Rect { x_min: 10, y_min: 0, x_max: 10, y_max: 0 });
    }

    fn rect(x_min: i16, y_min: i16, x_max: i16, y_max: i16) -> Rect {
        Rect { x_min, y_min, x_max, y_max }
    }

    macro_rules! test_cs_with_subrs {
        ($name:ident, $glob:expr, $loc:expr, $values:expr, $path:expr, $rect_res:expr) => {
            #[test]
            fn $name() {
                let data = gen_cff($glob, $loc, $values);
                let metadata = parse_metadata(&data).unwrap();
                let mut builder = Builder(String::new());
                let char_str = metadata.char_strings.get(0).unwrap();
                let rect = parse_char_string(char_str, &metadata, GlyphId(0), &mut builder).unwrap();

                assert_eq!(builder.0, $path);
                assert_eq!(rect, $rect_res);
            }
        };
    }

    macro_rules! test_cs {
        ($name:ident, $values:expr, $path:expr, $rect_res:expr) => {
            test_cs_with_subrs!($name, &[], &[], $values, $path, $rect_res);
        };
    }

    macro_rules! test_cs_err {
        ($name:ident, $values:expr, $err:expr) => {
            #[test]
            fn $name() {
                let data = gen_cff(&[], &[], $values);
                let metadata = parse_metadata(&data).unwrap();
                let mut builder = Builder(String::new());
                let char_str = metadata.char_strings.get(0).unwrap();
                let res = parse_char_string(char_str, &metadata, GlyphId(0), &mut builder);

                assert_eq!(res.unwrap_err(), $err);
            }
        };
    }

    test_cs!(move_to, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 Z ",
        rect(10, 20, 10, 20)
    );

    test_cs!(move_to_with_width, &[
        CFFInt(5), CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 Z ",
        rect(10, 20, 10, 20)
    );

    test_cs!(hmove_to, &[
        CFFInt(10), UInt8(operator::HORIZONTAL_MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 0 Z ",
        rect(10, 0, 10, 0)
    );

    test_cs!(hmove_to_with_width, &[
        CFFInt(10), CFFInt(20), UInt8(operator::HORIZONTAL_MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 20 0 Z ",
        rect(20, 0, 20, 0)
    );

    test_cs!(vmove_to, &[
        CFFInt(10), UInt8(operator::VERTICAL_MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 0 10 Z ",
        rect(0, 10, 0, 10)
    );

    test_cs!(vmove_to_with_width, &[
        CFFInt(10), CFFInt(20), UInt8(operator::VERTICAL_MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 0 20 Z ",
        rect(0, 20, 0, 20)
    );

    test_cs!(line_to, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), UInt8(operator::LINE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 L 40 60 Z ",
        rect(10, 20, 40, 60)
    );

    test_cs!(line_to_with_multiple_pairs, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(60), UInt8(operator::LINE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 L 40 60 L 90 120 Z ",
        rect(10, 20, 90, 120)
    );

    test_cs!(hline_to, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), UInt8(operator::HORIZONTAL_LINE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 L 40 20 Z ",
        rect(10, 20, 40, 20)
    );

    test_cs!(hline_to_with_two_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), UInt8(operator::HORIZONTAL_LINE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 L 40 20 L 40 60 Z ",
        rect(10, 20, 40, 60)
    );

    test_cs!(hline_to_with_three_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), UInt8(operator::HORIZONTAL_LINE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 L 40 20 L 40 60 L 90 60 Z ",
        rect(10, 20, 90, 60)
    );

    test_cs!(vline_to, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), UInt8(operator::VERTICAL_LINE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 L 10 50 Z ",
        rect(10, 20, 10, 50)
    );

    test_cs!(vline_to_with_two_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), UInt8(operator::VERTICAL_LINE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 L 10 50 L 50 50 Z ",
        rect(10, 20, 50, 50)
    );

    test_cs!(vline_to_with_three_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), UInt8(operator::VERTICAL_LINE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 L 10 50 L 50 50 L 50 100 Z ",
        rect(10, 20, 50, 100)
    );

    test_cs!(curve_to, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(60), CFFInt(70), CFFInt(80),
        UInt8(operator::CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 C 40 60 90 120 160 200 Z ",
        rect(10, 20, 160, 200)
    );

    test_cs!(curve_to_with_two_sets_of_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(60), CFFInt(70), CFFInt(80),
        CFFInt(90), CFFInt(100), CFFInt(110), CFFInt(120), CFFInt(130), CFFInt(140),
        UInt8(operator::CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 C 40 60 90 120 160 200 C 250 300 360 420 490 560 Z ",
        rect(10, 20, 490, 560)
    );

    test_cs!(hh_curve_to, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(60), UInt8(operator::HH_CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 C 40 20 80 70 140 70 Z ",
        rect(10, 20, 140, 70)
    );

    test_cs!(hh_curve_to_with_y, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(60), CFFInt(70), UInt8(operator::HH_CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 C 50 50 100 110 170 110 Z ",
        rect(10, 20, 170, 110)
    );

    test_cs!(vv_curve_to, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(60), UInt8(operator::VV_CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 C 10 50 50 100 50 160 Z ",
        rect(10, 20, 50, 160)
    );

    test_cs!(vv_curve_to_with_x, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(60), CFFInt(70), UInt8(operator::VV_CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], "M 10 20 C 40 60 90 120 90 190 Z ",
        rect(10, 20, 90, 190)
    );

    #[test]
    fn only_endchar() {
        let data = gen_cff(&[], &[], &[UInt8(operator::ENDCHAR)]);
        let metadata = parse_metadata(&data).unwrap();
        let mut builder = Builder(String::new());
        let char_str = metadata.char_strings.get(0).unwrap();
        assert!(parse_char_string(char_str, &metadata, GlyphId(0), &mut builder).is_err());
    }

    test_cs_with_subrs!(local_subr,
        &[],
        &[&[
            CFFInt(30),
            CFFInt(40),
            UInt8(operator::LINE_TO),
            UInt8(operator::RETURN),
        ]],
        &[
            CFFInt(10),
            UInt8(operator::HORIZONTAL_MOVE_TO),
            CFFInt(0 - 107), // subr index - subr bias
            UInt8(operator::CALL_LOCAL_SUBROUTINE),
            UInt8(operator::ENDCHAR),
        ],
        "M 10 0 L 40 40 Z ",
        rect(10, 0, 40, 40)
    );

    test_cs_with_subrs!(endchar_in_subr,
        &[],
        &[&[
            CFFInt(30),
            CFFInt(40),
            UInt8(operator::LINE_TO),
            UInt8(operator::ENDCHAR),
        ]],
        &[
            CFFInt(10),
            UInt8(operator::HORIZONTAL_MOVE_TO),
            CFFInt(0 - 107), // subr index - subr bias
            UInt8(operator::CALL_LOCAL_SUBROUTINE),
        ],
        "M 10 0 L 40 40 Z ",
        rect(10, 0, 40, 40)
    );

    test_cs_with_subrs!(global_subr,
        &[&[
            CFFInt(30),
            CFFInt(40),
            UInt8(operator::LINE_TO),
            UInt8(operator::RETURN),
        ]],
        &[],
        &[
            CFFInt(10),
            UInt8(operator::HORIZONTAL_MOVE_TO),
            CFFInt(0 - 107), // subr index - subr bias
            UInt8(operator::CALL_GLOBAL_SUBROUTINE),
            UInt8(operator::ENDCHAR),
        ],
        "M 10 0 L 40 40 Z ",
        rect(10, 0, 40, 40)
    );

    test_cs_err!(reserved_operator, &[
        CFFInt(10), UInt8(2),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidOperator);

    test_cs_err!(line_to_without_move_to, &[
        CFFInt(10), CFFInt(20), UInt8(operator::LINE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::MissingMoveTo);

    // Width must be set only once.
    test_cs_err!(two_vmove_to_with_width, &[
        CFFInt(10), CFFInt(20), UInt8(operator::VERTICAL_MOVE_TO),
        CFFInt(10), CFFInt(20), UInt8(operator::VERTICAL_MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(move_to_with_too_many_coords, &[
        CFFInt(10), CFFInt(10), CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(move_to_with_not_enought_coords, &[
        CFFInt(10), UInt8(operator::MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(hmove_to_with_too_many_coords, &[
        CFFInt(10), CFFInt(10), CFFInt(10), UInt8(operator::HORIZONTAL_MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(hmove_to_with_not_enought_coords, &[
        UInt8(operator::HORIZONTAL_MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(vmove_to_with_too_many_coords, &[
        CFFInt(10), CFFInt(10), CFFInt(10), UInt8(operator::VERTICAL_MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(vmove_to_with_not_enought_coords, &[
        UInt8(operator::VERTICAL_MOVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(line_to_with_single_coord, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), UInt8(operator::LINE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(line_to_with_odd_number_of_coord, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), UInt8(operator::LINE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(hline_to_without_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        UInt8(operator::HORIZONTAL_LINE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(vline_to_without_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        UInt8(operator::VERTICAL_LINE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(curve_to_with_invalid_num_of_coords_1, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(60), UInt8(operator::CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(curve_to_with_invalid_num_of_coords_2, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(60), CFFInt(70), CFFInt(80), CFFInt(90),
        UInt8(operator::CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(hh_curve_to_with_not_enought_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), UInt8(operator::HH_CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(hh_curve_to_with_too_many_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(30), CFFInt(40), CFFInt(50),
        UInt8(operator::HH_CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(vv_curve_to_with_not_enought_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), UInt8(operator::VV_CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(vv_curve_to_with_too_many_coords, &[
        CFFInt(10), CFFInt(20), UInt8(operator::MOVE_TO),
        CFFInt(30), CFFInt(40), CFFInt(50), CFFInt(30), CFFInt(40), CFFInt(50),
        UInt8(operator::VV_CURVE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::InvalidArgumentsStackLength);

    test_cs_err!(multiple_endchar, &[
        UInt8(operator::ENDCHAR),
        UInt8(operator::ENDCHAR),
    ], CFFError::DataAfterEndChar);

    test_cs_err!(operands_overflow, &[
        CFFInt(0), CFFInt(1), CFFInt(2), CFFInt(3), CFFInt(4), CFFInt(5), CFFInt(6), CFFInt(7), CFFInt(8), CFFInt(9),
        CFFInt(0), CFFInt(1), CFFInt(2), CFFInt(3), CFFInt(4), CFFInt(5), CFFInt(6), CFFInt(7), CFFInt(8), CFFInt(9),
        CFFInt(0), CFFInt(1), CFFInt(2), CFFInt(3), CFFInt(4), CFFInt(5), CFFInt(6), CFFInt(7), CFFInt(8), CFFInt(9),
        CFFInt(0), CFFInt(1), CFFInt(2), CFFInt(3), CFFInt(4), CFFInt(5), CFFInt(6), CFFInt(7), CFFInt(8), CFFInt(9),
        CFFInt(0), CFFInt(1), CFFInt(2), CFFInt(3), CFFInt(4), CFFInt(5), CFFInt(6), CFFInt(7), CFFInt(8), CFFInt(9),
    ], CFFError::ArgumentsStackLimitReached);

    test_cs_err!(operands_overflow_with_4_byte_ints, &[
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
        CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000), CFFInt(30000),
    ], CFFError::ArgumentsStackLimitReached);

    test_cs_err!(bbox_overflow, &[
        CFFInt(32767), UInt8(operator::HORIZONTAL_MOVE_TO),
        CFFInt(32767), UInt8(operator::HORIZONTAL_LINE_TO),
        UInt8(operator::ENDCHAR),
    ], CFFError::BboxOverflow);

    #[test]
    fn endchar_in_subr_with_extra_data_1() {
        let data = gen_cff(
            &[],
            &[&[
                CFFInt(30),
                CFFInt(40),
                UInt8(operator::LINE_TO),
                UInt8(operator::ENDCHAR),
            ]],
            &[
                CFFInt(10),
                UInt8(operator::HORIZONTAL_MOVE_TO),
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_LOCAL_SUBROUTINE),
                CFFInt(30),
                CFFInt(40),
                UInt8(operator::LINE_TO),
            ]
        );

        let metadata = parse_metadata(&data).unwrap();
        let mut builder = Builder(String::new());
        let char_str = metadata.char_strings.get(0).unwrap();
        let res = parse_char_string(char_str, &metadata, GlyphId(0), &mut builder);
        assert_eq!(res.unwrap_err(), CFFError::DataAfterEndChar);
    }

    #[test]
    fn endchar_in_subr_with_extra_data_2() {
        let data = gen_cff(
            &[],
            &[&[
                CFFInt(30),
                CFFInt(40),
                UInt8(operator::LINE_TO),
                UInt8(operator::ENDCHAR),
                CFFInt(30),
                CFFInt(40),
                UInt8(operator::LINE_TO),
            ]],
            &[
                CFFInt(10),
                UInt8(operator::HORIZONTAL_MOVE_TO),
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_LOCAL_SUBROUTINE),
            ]
        );

        let metadata = parse_metadata(&data).unwrap();
        let mut builder = Builder(String::new());
        let char_str = metadata.char_strings.get(0).unwrap();
        let res = parse_char_string(char_str, &metadata, GlyphId(0), &mut builder);
        assert_eq!(res.unwrap_err(), CFFError::DataAfterEndChar);
    }

    #[test]
    fn subr_without_return() {
        let data = gen_cff(
            &[],
            &[&[
                CFFInt(30),
                CFFInt(40),
                UInt8(operator::LINE_TO),
                UInt8(operator::ENDCHAR),
                CFFInt(30),
                CFFInt(40),
                UInt8(operator::LINE_TO),
            ]],
            &[
                CFFInt(10),
                UInt8(operator::HORIZONTAL_MOVE_TO),
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_LOCAL_SUBROUTINE),
            ]
        );

        let metadata = parse_metadata(&data).unwrap();
        let mut builder = Builder(String::new());
        let char_str = metadata.char_strings.get(0).unwrap();
        let res = parse_char_string(char_str, &metadata, GlyphId(0), &mut builder);
        assert_eq!(res.unwrap_err(), CFFError::DataAfterEndChar);
    }

    #[test]
    fn recursive_local_subr() {
        let data = gen_cff(
            &[],
            &[&[
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_LOCAL_SUBROUTINE),
            ]],
            &[
                CFFInt(10),
                UInt8(operator::HORIZONTAL_MOVE_TO),
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_LOCAL_SUBROUTINE),
            ]
        );

        let metadata = parse_metadata(&data).unwrap();
        let mut builder = Builder(String::new());
        let char_str = metadata.char_strings.get(0).unwrap();
        let res = parse_char_string(char_str, &metadata, GlyphId(0), &mut builder);
        assert_eq!(res.unwrap_err(), CFFError::NestingLimitReached);
    }

    #[test]
    fn recursive_global_subr() {
        let data = gen_cff(
            &[&[
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_GLOBAL_SUBROUTINE),
            ]],
            &[],
            &[
                CFFInt(10),
                UInt8(operator::HORIZONTAL_MOVE_TO),
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_GLOBAL_SUBROUTINE),
            ]
        );

        let metadata = parse_metadata(&data).unwrap();
        let mut builder = Builder(String::new());
        let char_str = metadata.char_strings.get(0).unwrap();
        let res = parse_char_string(char_str, &metadata, GlyphId(0), &mut builder);
        assert_eq!(res.unwrap_err(), CFFError::NestingLimitReached);
    }

    #[test]
    fn recursive_mixed_subr() {
        let data = gen_cff(
            &[&[
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_LOCAL_SUBROUTINE),
            ]],
            &[&[
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_GLOBAL_SUBROUTINE),
            ]],
            &[
                CFFInt(10),
                UInt8(operator::HORIZONTAL_MOVE_TO),
                CFFInt(0 - 107), // subr index - subr bias
                UInt8(operator::CALL_GLOBAL_SUBROUTINE),
            ]
        );

        let metadata = parse_metadata(&data).unwrap();
        let mut builder = Builder(String::new());
        let char_str = metadata.char_strings.get(0).unwrap();
        let res = parse_char_string(char_str, &metadata, GlyphId(0), &mut builder);
        assert_eq!(res.unwrap_err(), CFFError::NestingLimitReached);
    }

    #[test]
    fn zero_char_string_offset() {
        let data = writer::convert(&[
            // Header
            UInt8(1), // major version
            UInt8(0), // minor version
            UInt8(4), // header size
            UInt8(0), // absolute offset

            // Name INDEX
            UInt16(0), // count

            // Top DICT
            // INDEX
            UInt16(1), // count
            UInt8(1), // offset size
            UInt8(1), // index[0]
            UInt8(3), // index[1]
            // Data
            CFFInt(0), // zero offset!
            UInt8(top_dict_operator::CHAR_STRINGS_OFFSET as u8),
        ]);

        assert!(parse_metadata(&data).is_none());
    }

    #[test]
    fn invalid_char_string_offset() {
        let data = writer::convert(&[
            // Header
            UInt8(1), // major version
            UInt8(0), // minor version
            UInt8(4), // header size
            UInt8(0), // absolute offset

            // Name INDEX
            UInt16(0), // count

            // Top DICT
            // INDEX
            UInt16(1), // count
            UInt8(1), // offset size
            UInt8(1), // index[0]
            UInt8(3), // index[1]
            // Data
            CFFInt(2), // invalid offset!
            UInt8(top_dict_operator::CHAR_STRINGS_OFFSET as u8),
        ]);

        assert!(parse_metadata(&data).is_none());
    }

    // TODO: return from main
    // TODO: return without endchar
    // TODO: data after return
    // TODO: recursive subr
    // TODO: HORIZONTAL_STEM
    // TODO: VERTICAL_STEM
    // TODO: HORIZONTAL_STEM_HINT_MASK
    // TODO: HINT_MASK
    // TODO: COUNTER_MASK
    // TODO: VERTICAL_STEM_HINT_MASK
    // TODO: CURVE_LINE
    // TODO: LINE_CURVE
    // TODO: VH_CURVE_TO
    // TODO: HFLEX
    // TODO: FLEX
    // TODO: HFLEX1
    // TODO: FLEX1

    #[test]
    fn private_dict_size_overflow() {
        let data = &[
            0x00, 0x01, // count: 1
            0x01, // offset size: 1
            0x01, // index [0]: 1
            0x0C, // index [1]: 14
            0x1D, 0x7F, 0xFF, 0xFF, 0xFF, // length: i32::MAX
            0x1D, 0x7F, 0xFF, 0xFF, 0xFF, // offset: i32::MAX
            0x12 // operator: 18 (private)
        ];

        let top_dict = parse_top_dict(&mut Stream::new(data)).unwrap();
        assert_eq!(top_dict.private_dict_range, Some(2147483647..4294967294));
    }

    #[test]
    fn private_dict_negative_char_strings_offset() {
        let data = &[
            0x00, 0x01, // count: 1
            0x01, // offset size: 1
            0x01, // index [0]: 1
            0x03, // index [1]: 3
            // Item 0
            0x8A, // offset: -1
            0x11, // operator: 17 (char_string)
        ];

        assert!(parse_top_dict(&mut Stream::new(data)).is_none());
    }

    #[test]
    fn private_dict_no_char_strings_offset_operand() {
        let data = &[
            0x00, 0x01, // count: 1
            0x01, // offset size: 1
            0x01, // index [0]: 1
            0x02, // index [1]: 2
            // Item 0
            // <-- No number here.
            0x11, // operator: 17 (char_string)
        ];

        assert!(parse_top_dict(&mut Stream::new(data)).is_none());
    }

    #[test]
    fn negative_private_dict_offset_and_size() {
        let data = &[
            0x00, 0x01, // count: 1
            0x01, // offset size: 1
            0x01, // index [0]: 1
            0x04, // index [1]: 4
            // Item 0
            0x8A, // length: -1
            0x8A, // offset: -1
            0x12, // operator: 18 (private)
        ];

        let top_dict = parse_top_dict(&mut Stream::new(data)).unwrap();
        assert!(top_dict.private_dict_range.is_none());
    }
}