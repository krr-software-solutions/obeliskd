//! `supernet status` and `subnet status`: an overview of each supernet (or subnet), and a grid of
//! its address space. A supernet's items are its subnets; a subnet's are its ips, and its grid also
//! shows Docker's slice (blue) and blocks with nothing obeliskd would hand out (grey).
//!
//! The grid splits a supernet into equal blocks, one cell each: free (green), all one subnet (red;
//! each subnet keeps one shade, and neighbouring subnets get different ones), or mixed (magenta:
//! part subnet and part free, or several subnets; the block is too coarse to show which).
//! Subnets that fill whole blocks are labelled in their cells: their name, or a numbered alias
//! (also in the table's ALIAS column) when the name doesn't fit. Without
//! colour (not a terminal, or `NO_COLOR`), cells are characters instead: `.` free, a letter per
//! subnet (`#` past `z`), `+` mixed.

use std::fmt::Write;

use ipnet::Ipv4Net;
use std::net::Ipv4Addr;

use obeliskd::control::api::{IpNode, SubnetNode, SupernetNode};
use obeliskd::model::SubnetKind;
use obeliskd::model::ip::{allocatable_count, usable_count};

/// At most this many cells per grid (32 × 32).
const MAX_CELLS: u32 = 1024;

const GREENS: [(u8, u8, u8); 2] = [(46, 160, 67), (38, 138, 57)];
const REDS: [(u8, u8, u8); 4] = [(214, 48, 49), (140, 20, 30), (240, 120, 110), (100, 10, 40)];
const MAGENTAS: [(u8, u8, u8); 4] = [
    (200, 60, 200),
    (130, 40, 150),
    (235, 140, 235),
    (95, 25, 100),
];

/// One supernet after another. `block` is the grid's block size as a prefix length (default: the
/// supernet's + 8, so 256 cells).
///
/// # Errors
///
/// A block size bigger than a supernet, or so small the grid would have more than 1024 cells.
pub fn render(
    supernets: &[SupernetNode],
    block: Option<u8>,
    color: bool,
) -> Result<String, String> {
    if supernets.is_empty() {
        return Ok("no supernets\n".to_string());
    }
    let mut out = String::new();
    for (i, supernet) in supernets.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let space = supernet_space(supernet);
        let layout = layout(&space, block)?;
        out.push_str(&overview(supernet, &layout));
        out.push_str(&grid(&space, &layout, color));
    }
    Ok(out)
}

/// A supernet's grid: its subnets are the items; everything else is free.
fn supernet_space(supernet: &SupernetNode) -> Space<'_> {
    let items: Vec<Item> = subnets(supernet)
        .iter()
        .map(|s| Item {
            name: s.name.as_str(),
            cidr: s.cidr,
        })
        .collect();
    let covered: Vec<Ipv4Net> = items.iter().map(|i| i.cidr).collect();
    Space {
        what: format!("supernet {}", supernet.name),
        cidr: supernet.cidr,
        item: "subnet",
        items,
        docker: None,
        free: Box::new(move |block| {
            size(block) - covered.iter().map(|c| overlap(*c, block)).sum::<u64>()
        }),
    }
}

/// `subnet status`: every subnet in `supernets`, one after another, with its ips. `block` is the
/// grid's block size (default: the subnet's prefix + 8, so one ip per cell up to a /24).
///
/// # Errors
///
/// A block size bigger than a subnet, or so small the grid would have more than 1024 cells.
pub fn render_subnets(
    supernets: &[SupernetNode],
    block: Option<u8>,
    color: bool,
) -> Result<String, String> {
    let all: Vec<(&SupernetNode, &SubnetNode)> = supernets
        .iter()
        .flat_map(|s| subnets(s).iter().map(move |sub| (s, sub)))
        .collect();
    if all.is_empty() {
        return Ok("no subnets\n".to_string());
    }
    let mut out = String::new();
    for (i, (supernet, subnet)) in all.into_iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let space = subnet_space(supernet, subnet);
        let layout = layout(&space, block)?;
        out.push_str(&subnet_overview(supernet, subnet, &layout));
        out.push_str(&grid(&space, &layout, color));
    }
    Ok(out)
}

fn ips_of(subnet: &SubnetNode) -> &[IpNode] {
    subnet.ips.as_deref().unwrap_or_default()
}

/// A subnet's grid: its ips are the items, Docker's slice is shown, and free means what `ip alloc`
/// could still hand out.
fn subnet_space<'a>(supernet: &SupernetNode, subnet: &'a SubnetNode) -> Space<'a> {
    let items: Vec<Item> = ips_of(subnet)
        .iter()
        .map(|ip| Item {
            name: ip.name.as_str(),
            cidr: Ipv4Net::from(ip.address),
        })
        .collect();
    let taken: Vec<Ipv4Net> = items.iter().map(|i| i.cidr).collect();
    Space {
        what: format!("subnet {}/{}", supernet.name, subnet.name),
        cidr: subnet.cidr,
        item: "ip",
        items,
        docker: subnet.docker_range,
        free: Box::new(move |block| {
            allocatable_in(subnet, block) - taken.iter().map(|t| overlap(*t, block)).sum::<u64>()
        }),
    }
}

/// How many addresses in `block` (inside the subnet) obeliskd could hand out: the same rules as
/// [`allocatable_count`], for a slice of the subnet.
fn allocatable_in(subnet: &SubnetNode, block: Ipv4Net) -> u64 {
    let usable = usable_count(block);
    match subnet.kind {
        SubnetKind::Host => usable,
        SubnetKind::Docker => {
            let docker = subnet.docker_range.map_or(0, |d| match overlap(d, block) {
                0 => 0,
                _ if d.contains(&block) => usable,
                _ => usable_count(d),
            });
            let in_docker = |a: Ipv4Addr| subnet.docker_range.is_some_and(|d| d.contains(&a));
            let edges = [subnet.cidr.network(), subnet.cidr.broadcast()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .filter(|a| {
                    block.contains(a) && !matches!(a.octets()[3], 0 | 255) && !in_docker(*a)
                })
                .count() as u64;
            usable - docker - edges
        }
    }
}

fn subnet_overview(supernet: &SupernetNode, subnet: &SubnetNode, layout: &Layout) -> String {
    let usable = allocatable_count(subnet.kind, subnet.cidr, subnet.docker_range);
    let used = ips_of(subnet).len();
    let mut out = format!(
        "subnet {}/{}  {}  {}  {} ips, {} usable, {} allocated ({})\n",
        supernet.name,
        subnet.name,
        subnet.kind,
        subnet.cidr,
        thousands(size(subnet.cidr)),
        thousands(usable),
        used,
        percent(used as u64, usable),
    );
    if let Some(range) = subnet.docker_range {
        let gateway = subnet
            .docker_gateway
            .map_or_else(String::new, |g| format!(", gateway {g}"));
        let _ = writeln!(out, "  docker's slice {range}{gateway}");
    }
    out.push('\n');
    let rows: Vec<[String; 5]> = ips_of(subnet)
        .iter()
        .zip(&layout.labels)
        .map(|(ip, label)| {
            [
                ip.name.to_string(),
                label
                    .alias
                    .map_or_else(|| "-".to_string(), |a| a.to_string()),
                ip.address.to_string(),
                ip.tag
                    .as_ref()
                    .map_or_else(|| "-".to_string(), ToString::to_string),
                ip.hosts.as_ref().map_or(0, Vec::len).to_string(),
            ]
        })
        .collect();
    out.push_str(&table(&["IP", "ALIAS", "ADDRESS", "TAG", "HOSTS"], &rows));
    out
}

fn subnets(supernet: &SupernetNode) -> &[SubnetNode] {
    supernet.subnets.as_deref().unwrap_or_default()
}

fn size(cidr: Ipv4Net) -> u64 {
    1u64 << (32 - cidr.prefix_len())
}

fn overview(supernet: &SupernetNode, layout: &Layout) -> String {
    let total = size(supernet.cidr);
    let covered: u64 = subnets(supernet).iter().map(|s| size(s.cidr)).sum();
    let ips: usize = subnets(supernet)
        .iter()
        .map(|s| s.ips.as_ref().map_or(0, Vec::len))
        .sum();
    let mut out = format!(
        "supernet {}  {}  {} ips, {} usable\n  {} subnet{}, covering {} ips ({}); {} allocated\n\n",
        supernet.name,
        supernet.cidr,
        thousands(total),
        thousands(usable_count(supernet.cidr)),
        subnets(supernet).len(),
        plural(subnets(supernet).len()),
        thousands(covered),
        percent(covered, total),
        ips,
    );
    let rows: Vec<[String; 8]> = subnets(supernet)
        .iter()
        .zip(&layout.labels)
        .map(|(s, label)| {
            let usable = allocatable_count(s.kind, s.cidr, s.docker_range);
            let used = s.ips.as_ref().map_or(0, Vec::len);
            [
                s.name.to_string(),
                label
                    .alias
                    .map_or_else(|| "-".to_string(), |a| a.to_string()),
                s.kind.to_string(),
                s.cidr.to_string(),
                thousands(size(s.cidr)),
                thousands(usable),
                format!("{used} ({})", percent(used as u64, usable)),
                percent(size(s.cidr), total),
            ]
        })
        .collect();
    out.push_str(&table(
        &[
            "SUBNET",
            "ALIAS",
            "KIND",
            "CIDR",
            "SIZE",
            "USABLE",
            "ALLOCATED",
            "OF SUPERNET",
        ],
        &rows,
    ));
    out
}

/// What a grid draws: a space (a supernet, or a subnet) and the items in it (its subnets, or its
/// ips as /32s), plus, for a subnet, Docker's slice and which addresses are free.
struct Space<'a> {
    /// For errors: `supernet obd`, `subnet obd/k3d-dev`.
    what: String,
    cidr: Ipv4Net,
    /// `subnet` or `ip`, for the legend.
    item: &'static str,
    items: Vec<Item<'a>>,
    docker: Option<Ipv4Net>,
    /// How many addresses in a block could still be handed out (not counting the items in it).
    free: Box<dyn Fn(Ipv4Net) -> u64 + 'a>,
}

struct Item<'a> {
    name: &'a str,
    cidr: Ipv4Net,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cell {
    Free,
    /// Wholly this item (its index), with nothing free.
    Item(usize),
    Mixed,
    /// Only Docker's slice (subnets).
    Docker,
    /// Nothing obeliskd would hand out, and no ips (subnets).
    Reserved,
}

/// How a grid is laid out, and what each item (by index) is labelled with in it.
struct Layout {
    /// The block size, as a prefix length.
    block: u8,
    cells: u32,
    columns: usize,
    labels: Vec<Label>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Label {
    /// A number standing in for a name too long for the item's cells.
    alias: Option<usize>,
    /// What's drawn in its cells (the name, else the alias), if anything fits.
    text: Option<String>,
}

/// Picks the block size, and labels: every item that fills at least one whole block gets its
/// name drawn in its cells, or, if the name doesn't fit, a numbered alias (smallest items first,
/// so they get the shortest numbers). An alias that still doesn't fit isn't drawn.
fn layout(space: &Space, block: Option<u8>) -> Result<Layout, String> {
    let prefix = space.cidr.prefix_len();
    let block = block.unwrap_or((prefix + 8).min(32));
    if block < prefix || block > 32 {
        return Err(format!(
            "--block-size {block} doesn't fit {} ({}): use {prefix} to 32",
            space.what, space.cidr
        ));
    }
    let cells = 1u32 << (block - prefix);
    if cells > MAX_CELLS {
        let smallest = prefix + 10;
        return Err(format!(
            "--block-size {block} makes {cells} blocks in {} ({}); at most {MAX_CELLS} (use {smallest} or less)",
            space.what, space.cidr
        ));
    }
    // Square-ish: as many columns as rows, or twice as many.
    let columns = 1usize << (cells.trailing_zeros().div_ceil(2));

    let mut labels = vec![Label::default(); space.items.len()];
    let mut aliased: Vec<(usize, usize)> = Vec::new(); // (item, its cells)
    for (n, item) in space.items.iter().enumerate() {
        if let Some(room) = room(item.cidr, block, columns) {
            if item.name.len() <= room {
                labels[n].text = Some(item.name.to_string());
            } else {
                aliased.push((n, 1usize << (block - item.cidr.prefix_len())));
            }
        }
    }
    aliased.sort_by_key(|&(n, cells)| (cells, space.items[n].cidr.network()));
    for (i, (n, _)) in aliased.into_iter().enumerate() {
        let alias = i + 1;
        labels[n].alias = Some(alias);
        let room = room(space.items[n].cidr, block, columns).expect("only full-block items");
        labels[n].text = (alias.to_string().len() <= room).then(|| alias.to_string());
    }
    Ok(Layout {
        block,
        cells,
        columns,
        labels,
    })
}

/// How many characters an item's label may take: two per cell, at most a row. `None` if it
/// doesn't fill a whole block.
fn room(cidr: Ipv4Net, block: u8, columns: usize) -> Option<usize> {
    (cidr.prefix_len() <= block).then(|| 2 * (1usize << (block - cidr.prefix_len())).min(columns))
}

/// Black or white, whichever reads better on `(r, g, b)`.
fn foreground((r, g, b): (u8, u8, u8)) -> (u8, u8, u8) {
    let luma = 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b);
    if luma > 150_000 {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    }
}

/// The addresses two ranges share.
fn overlap(a: Ipv4Net, b: Ipv4Net) -> u64 {
    if a.contains(&b) {
        size(b)
    } else if b.contains(&a) {
        size(a)
    } else {
        0
    }
}

/// What a block holds: one item and nothing free is that item; nothing but free space (ignoring
/// addresses that are never handed out) is free; no items and nothing free is Docker's (if any of
/// it is) or reserved; anything else is mixed.
fn classify(space: &Space, block: Ipv4Net) -> Cell {
    let items: Vec<usize> = space
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| overlap(item.cidr, block) > 0)
        .map(|(n, _)| n)
        .collect();
    let free = (space.free)(block) > 0;
    let docker = space.docker.is_some_and(|d| overlap(d, block) > 0);
    match (items.as_slice(), free, docker) {
        ([], false, true) => Cell::Docker,
        ([], false, false) => Cell::Reserved,
        ([], true, false) => Cell::Free,
        ([n], false, false) => Cell::Item(*n),
        _ => Cell::Mixed,
    }
}

const BLUES: [(u8, u8, u8); 2] = [(52, 101, 164), (44, 86, 140)];
const GREY: (u8, u8, u8) = (88, 88, 88);

fn grid(space: &Space, layout: &Layout, color: bool) -> String {
    let Layout {
        block,
        cells,
        columns,
        ..
    } = *layout;
    let block_size = 1u64 << (32 - block);
    let start = u64::from(space.cidr.network().to_bits());
    let block_at = |i: u64| {
        let first = u32::try_from(start + i * block_size).expect("inside IPv4");
        Ipv4Net::new(std::net::Ipv4Addr::from_bits(first), block).expect("a valid prefix")
    };
    let kinds: Vec<Cell> = (0..u64::from(cells))
        .map(|i| classify(space, block_at(i)))
        .collect();
    let shades = shade(&kinds, columns, space.items.len());

    // Labels, centred in the middle row of each item's cells: a character per position.
    let mut overlay: Vec<Vec<Option<char>>> = vec![vec![None; 2 * columns]; kinds.len() / columns];
    for (item, label) in space.items.iter().zip(&layout.labels) {
        let Some(text) = &label.text else { continue };
        let first =
            usize::try_from((u64::from(item.cidr.network().to_bits()) - start) / block_size)
                .expect("at most 1024 cells");
        let count = 1usize << (block - item.cidr.prefix_len());
        let rows = count.div_ceil(columns);
        let row = first / columns + (rows - 1) / 2;
        let width = 2 * count.min(columns);
        let from = 2 * (first % columns) + (width - text.len()) / 2;
        for (k, c) in text.chars().enumerate() {
            overlay[row][from + k] = Some(c);
        }
    }

    let ips = |n: u64| format!("{} ip{}", thousands(n), if n == 1 { "" } else { "s" });
    let row_prefix = block - u8::try_from(columns.trailing_zeros()).expect("at most 32 columns");
    let mut out = format!(
        "\n  [ grid: {} × {} blocks ]  [ row: /{row_prefix}, {} ]  [ block: /{block}, {} ]\n",
        columns,
        kinds.len() / columns,
        ips(block_size * columns as u64),
        ips(block_size),
    );
    let label_width = 15;
    for (row, chunk) in kinds.chunks(columns).enumerate() {
        let first = block_at((row * columns) as u64).network();
        let _ = write!(out, "  {:<label_width$} ", first.to_string());
        for (column, cell) in chunk.iter().enumerate() {
            let i = row * columns + column;
            let checker = (row + column) % 2;
            let text = |fill: char| {
                let at = |k: usize| overlay[row][2 * column + k].unwrap_or(fill);
                format!("{}{}", at(0), at(1))
            };
            if color {
                let bg = match cell {
                    Cell::Free => GREENS[checker],
                    Cell::Item(_) => REDS[shades[i]],
                    Cell::Mixed => MAGENTAS[shades[i]],
                    Cell::Docker => BLUES[checker],
                    Cell::Reserved => GREY,
                };
                let ((r, g, b), (fr, fg, fb)) = (bg, foreground(bg));
                let _ = write!(
                    out,
                    "\x1b[48;2;{r};{g};{b};38;2;{fr};{fg};{fb}m{}\x1b[0m",
                    text(' ')
                );
            } else {
                let fill = match cell {
                    Cell::Free => '.',
                    Cell::Item(n) => u8::try_from(*n)
                        .ok()
                        .filter(|n| *n < 26)
                        .map_or('#', |n| char::from(b'a' + n)),
                    Cell::Mixed => '+',
                    Cell::Docker => '~',
                    Cell::Reserved => '-',
                };
                out.push_str(&text(fill));
            }
        }
        out.push('\n');
    }
    out.push_str(&legend(space, &kinds, color));
    out
}

/// A shade per cell, so neighbouring areas (left/right, above/below) don't run together: each
/// item gets one red shade, different from every item it touches; each mixed cell gets a magenta
/// shade different from the mixed cells next to it. Other cells don't use this.
fn shade(kinds: &[Cell], columns: usize, items: usize) -> Vec<usize> {
    let neighbours = |i: usize| {
        let mut out = Vec::new();
        if i % columns > 0 {
            out.push(i - 1);
        }
        if i % columns + 1 < columns && i + 1 < kinds.len() {
            out.push(i + 1);
        }
        if i >= columns {
            out.push(i - columns);
        }
        if i + columns < kinds.len() {
            out.push(i + columns);
        }
        out
    };

    // Items: which ones touch, then the lowest shade none of the earlier ones it touches has.
    let mut touches = vec![Vec::new(); items];
    for (i, cell) in kinds.iter().enumerate() {
        if let Cell::Item(a) = cell {
            for j in neighbours(i) {
                if let Cell::Item(b) = kinds[j] {
                    if b != *a && !touches[*a].contains(&b) {
                        touches[*a].push(b);
                    }
                }
            }
        }
    }
    let mut item_shade = vec![0; items];
    for a in 0..items {
        let taken: Vec<usize> = touches[a]
            .iter()
            .filter(|b| **b < a)
            .map(|b| item_shade[*b])
            .collect();
        item_shade[a] = (0..REDS.len())
            .find(|s| !taken.contains(s))
            .unwrap_or(a % REDS.len());
    }

    // Mixed cells: in order, avoiding the shades of the ones before them (left and above).
    let mut shades = vec![0; kinds.len()];
    for i in 0..kinds.len() {
        shades[i] = match kinds[i] {
            Cell::Item(n) => item_shade[n],
            Cell::Mixed => {
                let taken: Vec<usize> = neighbours(i)
                    .into_iter()
                    .filter(|&j| j < i && kinds[j] == Cell::Mixed)
                    .map(|j| shades[j])
                    .collect();
                (0..MAGENTAS.len())
                    .find(|s| !taken.contains(s))
                    .expect("at most two earlier neighbours")
            }
            _ => 0,
        };
    }
    shades
}

/// Docker and reserved only show up in the legend when the grid has them.
fn legend(space: &Space, kinds: &[Cell], color: bool) -> String {
    let has = |cell: Cell| kinds.contains(&cell);
    let item = space.item;
    if color {
        let swatch = |(r, g, b): (u8, u8, u8)| format!("\x1b[48;2;{r};{g};{b}m  \x1b[0m");
        let mut out = format!(
            "\n  {} free   {} {item}   {} mixed (part {item}, part free, or several {item}s)",
            swatch(GREENS[0]),
            swatch(REDS[0]),
            swatch(MAGENTAS[0])
        );
        if has(Cell::Docker) {
            let _ = write!(out, "   {} docker's slice", swatch(BLUES[0]));
        }
        if has(Cell::Reserved) {
            let _ = write!(out, "   {} never handed out", swatch(GREY));
        }
        out.push('\n');
        out
    } else {
        let mut keys = vec![".. free".to_string(), "++ mixed".to_string()];
        if has(Cell::Docker) {
            keys.push("~~ docker's slice".to_string());
        }
        if has(Cell::Reserved) {
            keys.push("-- never handed out".to_string());
        }
        keys.extend(space.items.iter().take(26).enumerate().map(|(n, it)| {
            let c = char::from(b'a' + u8::try_from(n).expect("at most 26"));
            format!("{c}{c} {}", it.name)
        }));
        if space.items.len() > 26 {
            keys.push(format!("## more {item}s"));
        }
        format!("\n  {}\n", keys.join("   "))
    }
}

fn table<const N: usize>(headers: &[&str; N], rows: &[[String; N]]) -> String {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: &[&str]| {
        let mut out = String::from("  ");
        for (i, (cell, w)) in cells.iter().zip(&widths).enumerate() {
            if i + 1 == N {
                out.push_str(cell);
            } else {
                let _ = write!(out, "{cell:<w$}  ");
            }
        }
        out.push('\n');
        out
    };
    let mut out = line(headers);
    for row in rows {
        out.push_str(&line(&row.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    out
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[allow(clippy::cast_precision_loss)] // percentages of at most 2^32 addresses
fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".to_string();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use obeliskd::control::api::IpNode;
    use obeliskd::model::{Name, SubnetKind};

    use super::*;

    fn name(s: &str) -> Name {
        Name::parse(s).unwrap()
    }

    fn subnet(
        n: &str,
        kind: SubnetKind,
        cidr: &str,
        range: Option<&str>,
        ips: usize,
    ) -> SubnetNode {
        SubnetNode {
            name: name(n),
            kind,
            cidr: cidr.parse().unwrap(),
            docker_range: range.map(|r| r.parse().unwrap()),
            docker_gateway: None,
            ips: Some(
                (0..ips)
                    .map(|i| IpNode {
                        name: name(&format!("i{i}")),
                        address: "10.57.0.1".parse().unwrap(),
                        tag: None,
                        hosts: None,
                    })
                    .collect(),
            ),
        }
    }

    /// A /24 supernet in /28 blocks (16 cells, 4 × 4): `a` fills blocks 0–1, `b` block 2, `c` is a
    /// /30 inside block 4, and `d`/`e` are two /29s sharing block 6.
    fn obd() -> SupernetNode {
        SupernetNode {
            name: name("obd"),
            cidr: "10.57.0.0/24".parse().unwrap(),
            subnets: Some(vec![
                subnet(
                    "a",
                    SubnetKind::Docker,
                    "10.57.0.0/27",
                    Some("10.57.0.0/29"),
                    2,
                ),
                subnet("b", SubnetKind::Host, "10.57.0.32/28", None, 0),
                subnet("c", SubnetKind::Host, "10.57.0.64/30", None, 1),
                subnet("d", SubnetKind::Host, "10.57.0.96/29", None, 0),
                subnet("e", SubnetKind::Host, "10.57.0.104/29", None, 0),
            ]),
        }
    }

    #[test]
    fn plain_grids_show_subnets_mixed_blocks_and_free_space() {
        let out = render(&[obd()], Some(28), false).unwrap();
        assert_eq!(
            out,
            "\
supernet obd  10.57.0.0/24  256 ips, 254 usable
  5 subnets, covering 68 ips (26.6%); 3 allocated

  SUBNET  ALIAS  KIND    CIDR            SIZE  USABLE  ALLOCATED  OF SUPERNET
  a       -      docker  10.57.0.0/27    32    23      2 (8.7%)   12.5%
  b       -      host    10.57.0.32/28   16    16      0 (0.0%)   6.2%
  c       -      host    10.57.0.64/30   4     4       1 (25.0%)  1.6%
  d       -      host    10.57.0.96/29   8     8       0 (0.0%)   3.1%
  e       -      host    10.57.0.104/29  8     8       0 (0.0%)   3.1%

  [ grid: 4 × 4 blocks ]  [ row: /26, 64 ips ]  [ block: /28, 16 ips ]
  10.57.0.0       aaaabb..
  10.57.0.64      ++..++..
  10.57.0.128     ........
  10.57.0.192     ........

  .. free   ++ mixed   aa a   bb b   cc c   dd d   ee e
"
        );
    }

    #[test]
    fn names_that_fit_are_drawn_and_the_rest_get_aliases_smallest_first() {
        // A /22 in /28 blocks: 64 cells, 8 × 8 (16 characters a row).
        let host = |n: &str, cidr: &str| subnet(n, SubnetKind::Host, cidr, None, 0);
        let mut subnets = vec![
            host("k3d-dev", "10.57.0.0/25"), // 8 cells, a row: fits
            host("a-much-too-long-name", "10.57.0.128/26"), // 4 cells, 8 chars: alias 13
            host("rather-long", "10.57.0.192/27"), // 2 cells, 4 chars: alias 12
            host("db", "10.57.0.224/28"),    // 1 cell, 2 chars: fits
            host("cache-1", "10.57.0.240/28"), // 1 cell: alias 1
            host("big-one", "10.57.1.0/24"), // 2 rows: fits, on the first
            host("tiny", "10.57.2.0/30"),    // inside a block: no label
        ];
        // Ten more single-cell subnets with long names: aliases 2–11 (two digits still fit a cell).
        for i in 0..10 {
            subnets.push(host(
                &format!("filler-{i}"),
                &format!("10.57.3.{}/28", 16 * i),
            ));
        }
        let wide = SupernetNode {
            name: name("wide"),
            cidr: "10.57.0.0/22".parse().unwrap(),
            subnets: Some(subnets),
        };
        let layout = layout(&supernet_space(&wide), Some(28)).unwrap();
        let aliases: Vec<Option<usize>> = layout.labels.iter().map(|l| l.alias).collect();
        let fillers = (2..=11).map(Some);
        let expected: Vec<Option<usize>> = [None, Some(13), Some(12), None, Some(1), None, None]
            .into_iter()
            .chain(fillers)
            .collect();
        assert_eq!(
            aliases, expected,
            "single cells first: they get the short numbers"
        );

        let out = render(&[wide], Some(28), false).unwrap();
        let grid: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.contains("[ grid:"))
            .skip(1)
            .take(8)
            .map(|l| l.split_whitespace().nth(1).unwrap())
            .collect();
        assert_eq!(
            grid,
            [
                "aaaak3d-devaaaaa",
                "bbb13bbbc12cdb1e",
                "ffffbig-onefffff",
                "ffffffffffffffff",
                "++..............",
                "................",
                "2h3i4j5k6l7m8n9o",
                "1011............",
            ]
        );
        assert!(
            out.contains("  a-much-too-long-name  13 "),
            "the ALIAS column\n{out}"
        );
    }

    #[test]
    fn an_alias_too_long_for_its_cells_is_not_drawn() {
        // 110 single-cell subnets (/30 blocks) with long names: aliases 1–110.
        let subnets = (0..110)
            .map(|i| {
                let cidr = format!("10.57.{}.{}/30", i / 64, 4 * (i % 64));
                subnet(&format!("long-name-{i}"), SubnetKind::Host, &cidr, None, 0)
            })
            .collect();
        let many = SupernetNode {
            name: name("many"),
            cidr: "10.57.0.0/22".parse().unwrap(),
            subnets: Some(subnets),
        };
        let layout = layout(&supernet_space(&many), Some(30)).unwrap();
        assert_eq!(layout.labels[98].text.as_deref(), Some("99"));
        assert_eq!(layout.labels[99].alias, Some(100));
        assert_eq!(
            layout.labels[99].text, None,
            "three digits in two characters"
        );
    }

    /// obd/dev: a docker /27 with Docker's slice the first /29, and three ips.
    fn dev() -> SupernetNode {
        let ip = |n: &str, last: u8, tag: Option<&str>, hosts: usize| IpNode {
            name: name(n),
            address: std::net::Ipv4Addr::new(10, 57, 0, last),
            tag: tag.map(name),
            hosts: Some(
                (0..hosts)
                    .map(|i| {
                        obeliskd::model::Hostname::parse(&format!("h{i}.obd.internal")).unwrap()
                    })
                    .collect(),
            ),
        };
        let mut dev = subnet(
            "dev",
            SubnetKind::Docker,
            "10.57.0.0/27",
            Some("10.57.0.0/29"),
            0,
        );
        dev.docker_gateway = Some("10.57.0.1".parse().unwrap());
        dev.ips = Some(vec![
            ip("db", 8, None, 2),
            ip("web-server-1", 9, Some("pr-42"), 0),
            ip("x", 11, None, 0),
        ]);
        SupernetNode {
            name: name("obd"),
            cidr: "10.57.0.0/16".parse().unwrap(),
            subnets: Some(vec![dev]),
        }
    }

    #[test]
    fn subnet_grids_show_ips_dockers_slice_and_what_is_never_handed_out() {
        let out = render_subnets(&[dev()], None, false).unwrap();
        assert_eq!(
            out,
            "\
subnet obd/dev  docker  10.57.0.0/27  32 ips, 23 usable, 3 allocated (13.0%)
  docker's slice 10.57.0.0/29, gateway 10.57.0.1

  IP            ALIAS  ADDRESS     TAG    HOSTS
  db            -      10.57.0.8   -      2
  web-server-1  1      10.57.0.9   pr-42  0
  x             -      10.57.0.11  -      0

  [ grid: 8 × 4 blocks ]  [ row: /29, 8 ips ]  [ block: /32, 1 ip ]
  10.57.0.0       ~~~~~~~~~~~~~~~~
  10.57.0.8       db1b..xc........
  10.57.0.16      ................
  10.57.0.24      ..............--

  .. free   ++ mixed   ~~ docker's slice   -- never handed out   aa db   bb web-server-1   cc x
"
        );
    }

    #[test]
    fn coarser_subnet_blocks_mix_and_ignore_what_is_never_handed_out() {
        let out = render_subnets(&[dev()], Some(30), false).unwrap();
        let grid: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.contains("[ grid:"))
            .skip(1)
            .take(2)
            .map(|l| l.split_whitespace().nth(1).unwrap())
            .collect();
        // .8–.11 holds ips and a free address; .28–.31 is free apart from the broadcast.
        assert_eq!(grid, ["~~~~++..", "........"]);
    }

    #[test]
    fn no_subnets_says_so() {
        let empty = SupernetNode {
            name: name("obd"),
            cidr: "10.57.0.0/16".parse().unwrap(),
            subnets: Some(Vec::new()),
        };
        assert_eq!(
            render_subnets(&[empty], None, false).unwrap(),
            "no subnets\n"
        );
    }

    #[test]
    fn allocatable_in_a_block_adds_up_to_the_subnet() {
        let dev = &dev().subnets.unwrap()[0];
        for block in [27, 28, 29, 30, 31, 32] {
            let total: u64 = dev
                .cidr
                .subnets(block)
                .unwrap()
                .map(|b| allocatable_in(dev, b))
                .sum();
            assert_eq!(total, 23, "/{block}");
        }
    }

    #[test]
    fn labels_are_black_or_white_by_background() {
        assert_eq!(foreground(REDS[2]), (0, 0, 0), "light red");
        assert_eq!(foreground(REDS[1]), (255, 255, 255), "dark red");
    }

    #[test]
    fn the_default_block_makes_256_cells() {
        let out = render(&[obd()], None, false).unwrap();
        assert!(
            out.contains("[ grid: 16 × 16 blocks ]  [ row: /28, 16 ips ]  [ block: /32, 1 ip ]\n"),
            "{out}"
        );
        let wide = SupernetNode {
            name: name("wide"),
            cidr: "10.0.0.0/8".parse().unwrap(),
            subnets: Some(Vec::new()),
        };
        let out = render(&[wide], None, false).unwrap();
        assert!(
            out.contains(
                "[ grid: 16 × 16 blocks ]  [ row: /12, 1,048,576 ips ]  [ block: /16, 65,536 ips ]"
            ),
            "{out}"
        );
        assert!(out.contains("16,777,216 ips, 16,646,144 usable"), "{out}");
    }

    #[test]
    fn block_sizes_must_fit() {
        assert!(
            render(&[obd()], Some(23), false)
                .unwrap_err()
                .contains("use 24 to 32")
        );
        let wide = SupernetNode {
            name: name("wide"),
            cidr: "10.0.0.0/8".parse().unwrap(),
            subnets: None,
        };
        let error = render(&[wide], Some(24), false).unwrap_err();
        assert!(
            error.contains("makes 65536 blocks") && error.contains("use 18 or less"),
            "{error}"
        );
    }

    #[test]
    fn touching_subnets_and_mixed_cells_get_different_shades() {
        // a | b side by side, c below a; mixed cells in a row.
        let kinds = [
            Cell::Item(0),
            Cell::Item(1),
            Cell::Mixed,
            Cell::Mixed,
            Cell::Item(2),
            Cell::Free,
            Cell::Mixed,
            Cell::Mixed,
        ];
        let shades = shade(&kinds, 4, 3);
        assert_ne!(shades[0], shades[1], "a and b touch");
        assert_ne!(shades[0], shades[4], "a and c touch");
        assert_ne!(shades[2], shades[3], "mixed side by side");
        assert_ne!(shades[2], shades[6], "mixed above/below");
        assert_ne!(shades[6], shades[7]);
    }

    #[test]
    fn colour_cells_are_rgb_backgrounds() {
        let out = render(&[obd()], Some(28), true).unwrap();
        assert!(out.contains("\x1b[48;2;46;160;67m  \x1b[0m"), "free");
        assert!(out.contains("\x1b[48;2;214;48;49m  \x1b[0m"), "a subnet");
        assert!(out.contains("\x1b[48;2;200;60;200m  \x1b[0m"), "mixed");
    }

    #[test]
    fn no_supernets_says_so() {
        assert_eq!(render(&[], None, true).unwrap(), "no supernets\n");
    }
}
