//! Entity-relationship diagrams: the tables of a schema with their foreign
//! keys, a layout that puts referenced tables left of the tables that point
//! to them, and exports to Mermaid, Graphviz DOT and SVG.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use crate::{ObjectPath, TableDetails};

/// A column as drawn in a table box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    /// Part of a foreign key.
    pub foreign_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub path: ObjectPath,
    pub columns: Vec<EntityColumn>,
    pub comment: Option<String>,
    /// Referenced from this schema but not part of it (another schema).
    pub external: bool,
}

/// How many child rows one parent row has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    /// The foreign key is unique: at most one child per parent.
    OneToOne,
    OneToMany,
}

/// A foreign key: `from` (the child) references `to` (the parent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub name: String,
    pub from: usize,
    pub from_columns: Vec<String>,
    pub to: usize,
    pub to_columns: Vec<String>,
    /// The child may have no parent (a nullable key column).
    pub optional: bool,
    pub cardinality: Cardinality,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagram {
    pub entities: Vec<Entity>,
    pub links: Vec<Link>,
}

impl Diagram {
    /// Builds a diagram of `tables` (all in one schema). Foreign keys to tables
    /// elsewhere add an `external` box with just the referenced columns.
    pub fn build(tables: &[(ObjectPath, TableDetails)]) -> Self {
        let mut d = Diagram::default();
        let mut index: HashMap<(String, String), usize> = HashMap::new();
        for (path, details) in tables {
            let fk_cols: Vec<&str> =
                details.foreign_keys.iter().flat_map(|f| f.columns.iter().map(String::as_str)).collect();
            index.insert((path.schema.clone(), path.name.clone()), d.entities.len());
            d.entities.push(Entity {
                path: path.clone(),
                columns: details
                    .columns
                    .iter()
                    .map(|c| EntityColumn {
                        name: c.name.clone(),
                        data_type: c.data_type.clone(),
                        nullable: c.nullable,
                        primary_key: c.primary_key,
                        foreign_key: fk_cols.contains(&c.name.as_str()),
                    })
                    .collect(),
                comment: details.comment.clone(),
                external: false,
            });
        }
        for (i, (path, details)) in tables.iter().enumerate() {
            for fk in &details.foreign_keys {
                // Engines without schemas report the database (or nothing) as the schema.
                let same = fk.ref_schema.is_empty() || fk.ref_schema == path.schema || fk.ref_schema == path.database;
                let schema = if same { path.schema.clone() } else { fk.ref_schema.clone() };
                let to = match index.get(&(schema.clone(), fk.ref_table.clone())) {
                    Some(&t) => t,
                    None => {
                        let t = d.entities.len();
                        index.insert((schema.clone(), fk.ref_table.clone()), t);
                        d.entities.push(Entity {
                            path: ObjectPath::new(&path.database, schema, &fk.ref_table),
                            columns: Vec::new(),
                            comment: None,
                            external: true,
                        });
                        t
                    }
                };
                if d.entities[to].external {
                    for c in &fk.ref_columns {
                        if !d.entities[to].columns.iter().any(|e| &e.name == c) {
                            d.entities[to].columns.push(EntityColumn {
                                name: c.clone(),
                                data_type: String::new(),
                                nullable: false,
                                primary_key: true,
                                foreign_key: false,
                            });
                        }
                    }
                }
                let optional =
                    fk.columns.iter().any(|c| details.columns.iter().any(|col| &col.name == c && col.nullable));
                let mut key = fk.columns.clone();
                key.sort();
                let unique = details.indexes.iter().any(|ix| {
                    let mut cols = ix.columns.clone();
                    cols.sort();
                    (ix.unique || ix.primary) && cols == key
                }) || {
                    let mut pk: Vec<String> = details.primary_key().iter().map(|s| s.to_string()).collect();
                    pk.sort();
                    !pk.is_empty() && pk == key
                };
                d.links.push(Link {
                    name: fk.name.clone(),
                    from: i,
                    from_columns: fk.columns.clone(),
                    to,
                    to_columns: fk.ref_columns.clone(),
                    optional,
                    cardinality: if unique { Cardinality::OneToOne } else { Cardinality::OneToMany },
                });
            }
        }
        d
    }

    /// Indexes of the entities linked to `i`, either way.
    pub fn neighbors(&self, i: usize) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .links
            .iter()
            .filter_map(|l| {
                if l.from == i {
                    Some(l.to)
                } else if l.to == i {
                    Some(l.from)
                } else {
                    None
                }
            })
            .filter(|&n| n != i)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Top-left corner of each entity's box.
pub type Positions = Vec<(f32, f32)>;

const GAP_X: f32 = 90.0;
const GAP_Y: f32 = 36.0;

/// Places the boxes (`sizes` are their width and height): each group of
/// linked tables in columns, parents left of children, ordered to cut down on
/// crossing lines; groups side by side, wrapping into rows; unlinked tables
/// in a grid at the end.
pub fn layout(d: &Diagram, sizes: &[(f32, f32)]) -> Positions {
    let n = d.entities.len();
    let mut pos = vec![(0.0, 0.0); n];
    if n == 0 {
        return pos;
    }
    // Groups of linked tables (union-find).
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for l in &d.links {
        let (a, b) = (find(&mut parent, l.from), find(&mut parent, l.to));
        parent[a] = b;
    }
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    let mut linked: Vec<Vec<usize>> = Vec::new();
    let mut lonely: Vec<usize> = Vec::new();
    for g in groups.into_values() {
        if g.len() == 1 {
            lonely.push(g[0]);
        } else {
            linked.push(g);
        }
    }
    linked.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
    lonely.sort_by(|a, b| d.entities[*a].path.name.cmp(&d.entities[*b].path.name));

    let total_area: f32 = sizes.iter().map(|(w, h)| (w + GAP_X) * (h + GAP_Y)).sum();
    let row_width = (total_area.sqrt() * 1.6).max(1200.0);
    let (mut x0, mut y0, mut row_h) = (0.0f32, 0.0f32, 0.0f32);
    for g in &linked {
        let local = layout_group(d, g, sizes);
        let w = g.iter().map(|&i| local[&i].0 + sizes[i].0).fold(0.0, f32::max);
        let h = g.iter().map(|&i| local[&i].1 + sizes[i].1).fold(0.0, f32::max);
        if x0 > 0.0 && x0 + w > row_width {
            x0 = 0.0;
            y0 += row_h + GAP_Y * 2.0;
            row_h = 0.0;
        }
        for &i in g {
            pos[i] = (x0 + local[&i].0, y0 + local[&i].1);
        }
        x0 += w + GAP_X * 1.5;
        row_h = row_h.max(h);
    }
    if !lonely.is_empty() {
        if x0 > 0.0 {
            y0 += row_h + GAP_Y * 2.0;
        }
        // A grid of lonely tables, rows aligned to the tallest box in each row.
        let cell_w = lonely.iter().map(|&i| sizes[i].0).fold(0.0, f32::max) + GAP_X / 2.0;
        let per_row = ((row_width / cell_w).floor() as usize).max(1);
        for chunk in lonely.chunks(per_row) {
            let h = chunk.iter().map(|&i| sizes[i].1).fold(0.0, f32::max);
            for (k, &i) in chunk.iter().enumerate() {
                pos[i] = (k as f32 * cell_w, y0);
            }
            y0 += h + GAP_Y;
        }
    }
    pos
}

/// Column layout of one group of linked tables.
fn layout_group(d: &Diagram, group: &[usize], sizes: &[(f32, f32)]) -> HashMap<usize, (f32, f32)> {
    let member: HashMap<usize, usize> = group.iter().enumerate().map(|(k, &i)| (i, k)).collect();
    let m = group.len();
    // child → parents, without self references.
    let mut parents: Vec<Vec<usize>> = vec![Vec::new(); m];
    for l in &d.links {
        if let (Some(&c), Some(&p)) = (member.get(&l.from), member.get(&l.to))
            && c != p
            && !parents[c].contains(&p)
        {
            parents[c].push(p);
        }
    }
    // Drop edges that close a cycle (depth-first, in name order for stable results).
    let mut order: Vec<usize> = (0..m).collect();
    order.sort_by(|a, b| d.entities[group[*a]].path.name.cmp(&d.entities[group[*b]].path.name));
    let mut state = vec![0u8; m]; // 0 new, 1 on stack, 2 done
    let mut dag: Vec<Vec<usize>> = vec![Vec::new(); m];
    fn visit(v: usize, parents: &[Vec<usize>], state: &mut [u8], dag: &mut [Vec<usize>]) {
        state[v] = 1;
        for &p in &parents[v] {
            match state[p] {
                0 => {
                    dag[v].push(p);
                    visit(p, parents, state, dag);
                }
                2 => dag[v].push(p),
                _ => {} // back edge: part of a cycle
            }
        }
        state[v] = 2;
    }
    for &v in &order {
        if state[v] == 0 {
            visit(v, &parents, &mut state, &mut dag);
        }
    }
    // Layer = longest chain of parents.
    let mut layer = vec![usize::MAX; m];
    fn depth(v: usize, dag: &[Vec<usize>], layer: &mut [usize]) -> usize {
        if layer[v] != usize::MAX {
            return layer[v];
        }
        let l = dag[v].iter().map(|&p| depth(p, dag, layer) + 1).max().unwrap_or(0);
        layer[v] = l;
        l
    }
    for v in 0..m {
        depth(v, &dag, &mut layer);
    }
    let layers_n = layer.iter().max().map_or(1, |l| l + 1);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); layers_n];
    for &v in &order {
        layers[layer[v]].push(v);
    }
    // Undirected neighbors for ordering.
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); m];
    for (c, ps) in parents.iter().enumerate() {
        for &p in ps {
            adj[c].push(p);
            adj[p].push(c);
        }
    }
    // Barycenter sweeps: order each layer by the mean position of its neighbors.
    let mut rank = vec![0.0f32; m];
    let set_rank = |layers: &Vec<Vec<usize>>, rank: &mut Vec<f32>| {
        for l in layers {
            for (k, &v) in l.iter().enumerate() {
                rank[v] = k as f32;
            }
        }
    };
    set_rank(&layers, &mut rank);
    for sweep in 0..8 {
        let range: Vec<usize> =
            if sweep % 2 == 0 { (1..layers_n).collect() } else { (0..layers_n.saturating_sub(1)).rev().collect() };
        for li in range {
            let near = if sweep % 2 == 0 { li - 1 } else { li + 1 };
            let mut keyed: Vec<(f32, usize)> = layers[li]
                .iter()
                .map(|&v| {
                    let ns: Vec<f32> = adj[v].iter().filter(|&&u| layer[u] == near).map(|&u| rank[u]).collect();
                    let key = if ns.is_empty() { rank[v] } else { ns.iter().sum::<f32>() / ns.len() as f32 };
                    (key, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            layers[li] = keyed.into_iter().map(|(_, v)| v).collect();
            set_rank(&layers, &mut rank);
        }
    }
    // Coordinates: columns as wide as their widest box, each column centered vertically.
    let heights: Vec<f32> = layers
        .iter()
        .map(|l| l.iter().map(|&v| sizes[group[v]].1).sum::<f32>() + GAP_Y * l.len().saturating_sub(1) as f32)
        .collect();
    let tallest = heights.iter().copied().fold(0.0, f32::max);
    let mut out = HashMap::new();
    let mut x = 0.0;
    for (li, l) in layers.iter().enumerate() {
        let w = l.iter().map(|&v| sizes[group[v]].0).fold(0.0, f32::max);
        let mut y = (tallest - heights[li]) / 2.0;
        for &v in l {
            out.insert(group[v], (x, y));
            y += sizes[group[v]].1 + GAP_Y;
        }
        x += w + GAP_X;
    }
    out
}

/// A name Mermaid accepts as a bare word.
fn mermaid_word(s: &str) -> String {
    let w: String = s.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect();
    let w = w.trim_matches('_').to_string();
    if w.is_empty() { "_".into() } else { w }
}

/// The diagram as a Mermaid `erDiagram`.
pub fn mermaid(d: &Diagram) -> String {
    let mut out = String::from("erDiagram\n");
    let names: Vec<String> = d.entities.iter().map(|e| mermaid_word(&e.path.name)).collect();
    for (e, name) in d.entities.iter().zip(&names) {
        let _ = writeln!(out, "    {name} {{");
        for c in &e.columns {
            let ty = if c.data_type.is_empty() { "key".to_string() } else { mermaid_word(&c.data_type) };
            let keys: Vec<&str> =
                [(c.primary_key, "PK"), (c.foreign_key, "FK")].iter().filter(|(on, _)| *on).map(|(_, k)| *k).collect();
            let _ = write!(out, "        {ty} {}", mermaid_word(&c.name));
            if !keys.is_empty() {
                let _ = write!(out, " {}", keys.join(","));
            }
            out.push('\n');
        }
        out.push_str("    }\n");
    }
    for l in &d.links {
        let parent = if l.optional { "|o" } else { "||" };
        let child = match l.cardinality {
            Cardinality::OneToOne => "o|",
            Cardinality::OneToMany => "o{",
        };
        let label = if l.name.is_empty() { l.from_columns.join(", ") } else { l.name.clone() };
        let _ =
            writeln!(out, "    {} {parent}--{child} {} : \"{}\"", names[l.to], names[l.from], label.replace('"', "'"));
    }
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The diagram as Graphviz DOT, with one HTML-label table per entity.
pub fn dot(d: &Diagram) -> String {
    let mut out = String::from(
        "digraph schema {\n  rankdir=RL;\n  node [shape=plaintext, fontname=\"Helvetica\", fontsize=11];\n  edge [arrowhead=none, arrowtail=crow, dir=back];\n",
    );
    for (i, e) in d.entities.iter().enumerate() {
        let _ = write!(
            out,
            "  t{i} [label=<<table border=\"1\" cellborder=\"0\" cellspacing=\"0\" cellpadding=\"4\"{}><tr><td colspan=\"2\" bgcolor=\"#e5e7eb\"><b>{}</b></td></tr>",
            if e.external { " style=\"dashed\"" } else { "" },
            html_escape(&e.path.name)
        );
        for (k, c) in e.columns.iter().enumerate() {
            let mark = if c.primary_key {
                "🔑 "
            } else if c.foreign_key {
                "→ "
            } else {
                ""
            };
            let _ = write!(
                out,
                "<tr><td port=\"c{k}\" align=\"left\">{mark}{}</td><td align=\"left\"><font color=\"#6b7280\">{}</font></td></tr>",
                html_escape(&c.name),
                html_escape(&c.data_type)
            );
        }
        out.push_str("</table>>];\n");
    }
    for l in &d.links {
        let port = |e: usize, col: Option<&String>| {
            col.and_then(|c| d.entities[e].columns.iter().position(|x| &x.name == c))
                .map(|k| format!(":c{k}"))
                .unwrap_or_default()
        };
        let _ = writeln!(
            out,
            "  t{}{} -> t{}{} [tooltip=\"{}\"{}];",
            l.from,
            port(l.from, l.from_columns.first()),
            l.to,
            port(l.to, l.to_columns.first()),
            html_escape(&l.name),
            if l.optional { ", style=dashed" } else { "" }
        );
    }
    out.push_str("}\n");
    out
}

/// Box geometry shared by the SVG export and the app's canvas.
pub mod geometry {
    pub const HEADER: f32 = 30.0;
    pub const ROW: f32 = 22.0;
    pub const PAD: f32 = 10.0;
    /// Approximate width of one character at the SVG's 12 px font.
    pub const CHAR: f32 = 7.0;

    /// Height of a box with `rows` columns.
    pub fn height(rows: usize) -> f32 {
        HEADER + ROW * rows.max(1) as f32 + 6.0
    }

    /// Y of the middle of column row `k` (or of the header for `None`).
    pub fn row_y(k: Option<usize>) -> f32 {
        match k {
            Some(k) => HEADER + ROW * k as f32 + ROW / 2.0 + 3.0,
            None => HEADER / 2.0,
        }
    }
}

/// Box sizes estimated from text lengths (for the SVG export).
pub fn estimated_sizes(d: &Diagram) -> Vec<(f32, f32)> {
    use geometry::*;
    d.entities
        .iter()
        .map(|e| {
            let name = e.path.name.chars().count() as f32 * (CHAR + 1.0);
            let cols = e
                .columns
                .iter()
                .map(|c| (c.name.chars().count() + c.data_type.chars().count() + 4) as f32 * CHAR)
                .fold(0.0, f32::max);
            ((name.max(cols) + PAD * 2.0 + 18.0).max(140.0), height(e.columns.len()))
        })
        .collect()
}

/// Where a link leaves the child box and enters the parent box, and the
/// horizontal direction each end faces (+1 right, -1 left).
pub fn link_ends(
    d: &Diagram,
    l: &Link,
    pos: &[(f32, f32)],
    sizes: &[(f32, f32)],
) -> ((f32, f32, f32), (f32, f32, f32)) {
    let row = |e: usize, col: Option<&String>| {
        geometry::row_y(col.and_then(|c| d.entities[e].columns.iter().position(|x| &x.name == c)))
    };
    let (fx, fy) = pos[l.from];
    let (tx, ty) = pos[l.to];
    let (fw, tw) = (sizes[l.from].0, sizes[l.to].0);
    let y1 = fy + row(l.from, l.from_columns.first());
    let y2 = ty + row(l.to, l.to_columns.first());
    if l.from == l.to {
        // A self reference loops out of the right side.
        return ((fx + fw, y1, 1.0), (tx + tw, y2, 1.0));
    }
    // Use the facing sides; boxes in the same column connect on their left.
    if fx >= tx + tw {
        ((fx, y1, -1.0), (tx + tw, y2, 1.0))
    } else if fx + fw <= tx {
        ((fx + fw, y1, 1.0), (tx, y2, -1.0))
    } else {
        ((fx, y1, -1.0), (tx, y2, -1.0))
    }
}

/// The diagram as a standalone SVG image.
pub fn svg(d: &Diagram) -> String {
    use geometry::*;
    let sizes = estimated_sizes(d);
    let pos = layout(d, &sizes);
    let margin = 24.0;
    let width = pos.iter().zip(&sizes).map(|(p, s)| p.0 + s.0).fold(0.0, f32::max) + margin * 2.0;
    let height = pos.iter().zip(&sizes).map(|(p, s)| p.1 + s.1).fold(0.0, f32::max) + margin * 2.0;
    let mut out = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" viewBox=\"{x:.0} {x:.0} {width:.0} {height:.0}\" font-family=\"Helvetica, Arial, sans-serif\" font-size=\"12\">\n<rect x=\"{x:.0}\" y=\"{x:.0}\" width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>\n",
        x = -margin
    );
    for l in &d.links {
        let ((x1, y1, s1), (x2, y2, s2)) = link_ends(d, l, &pos, &sizes);
        let bend = ((x2 - x1).abs() / 2.0).max(40.0);
        let dash = if l.optional { " stroke-dasharray=\"5 4\"" } else { "" };
        let _ = writeln!(
            out,
            "<path d=\"M {x1:.1} {y1:.1} C {:.1} {y1:.1}, {:.1} {y2:.1}, {x2:.1} {y2:.1}\" fill=\"none\" stroke=\"#64748b\" stroke-width=\"1.3\"{dash}><title>{}</title></path>",
            x1 + s1 * bend,
            x2 + s2 * bend,
            html_escape(&l.name)
        );
        // Crow's foot on the child end for one-to-many, a bar on the parent end.
        if l.cardinality == Cardinality::OneToMany {
            let fx = x1 + s1 * 10.0;
            let _ = writeln!(
                out,
                "<path d=\"M {fx:.1} {y1:.1} L {x1:.1} {:.1} M {fx:.1} {y1:.1} L {x1:.1} {:.1}\" stroke=\"#64748b\" stroke-width=\"1.3\"/>",
                y1 - 5.0,
                y1 + 5.0
            );
        }
        let bx = x2 + s2 * 6.0;
        let _ = writeln!(
            out,
            "<path d=\"M {bx:.1} {:.1} L {bx:.1} {:.1}\" stroke=\"#64748b\" stroke-width=\"1.3\"/>",
            y2 - 5.0,
            y2 + 5.0
        );
    }
    for (i, e) in d.entities.iter().enumerate() {
        let (x, y) = pos[i];
        let (w, h) = sizes[i];
        let dash = if e.external { " stroke-dasharray=\"4 3\"" } else { "" };
        let _ = writeln!(
            out,
            "<g><rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"6\" fill=\"#ffffff\" stroke=\"#cbd5e1\"{dash}/>\n<path d=\"M {x:.1} {:.1} a 6 6 0 0 1 6 -6 h {:.1} a 6 6 0 0 1 6 6 v {:.1} h {:.1} z\" fill=\"#f1f5f9\"/>\n<text x=\"{:.1}\" y=\"{:.1}\" font-weight=\"bold\" fill=\"#0f172a\">{}</text>",
            y + 6.0,
            w - 12.0,
            HEADER - 6.0,
            -w,
            x + PAD,
            y + HEADER / 2.0 + 4.0,
            html_escape(&e.path.name)
        );
        for (k, c) in e.columns.iter().enumerate() {
            let cy = y + row_y(Some(k)) + 4.0;
            let mark = if c.primary_key {
                "#d97706"
            } else if c.foreign_key {
                "#2563eb"
            } else {
                "#cbd5e1"
            };
            let _ = writeln!(
                out,
                "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"3\" fill=\"{mark}\"/><text x=\"{:.1}\" y=\"{cy:.1}\" fill=\"#0f172a\"{}>{}</text><text x=\"{:.1}\" y=\"{cy:.1}\" fill=\"#64748b\" text-anchor=\"end\">{}</text>",
                x + PAD + 2.0,
                cy - 4.0,
                x + PAD + 10.0,
                if c.primary_key { " font-weight=\"bold\"" } else { "" },
                html_escape(&c.name),
                x + w - PAD,
                html_escape(&c.data_type)
            );
        }
        out.push_str("</g>\n");
    }
    out.push_str("</svg>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColumnInfo, ForeignKeyInfo, IndexInfo};

    fn col(name: &str, ty: &str, pk: bool, nullable: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), primary_key: pk, nullable, ..Default::default() }
    }

    fn fk(name: &str, cols: &[&str], schema: &str, table: &str, refs: &[&str]) -> ForeignKeyInfo {
        ForeignKeyInfo {
            name: name.into(),
            columns: cols.iter().map(|s| s.to_string()).collect(),
            ref_schema: schema.into(),
            ref_table: table.into(),
            ref_columns: refs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn shop() -> Vec<(ObjectPath, TableDetails)> {
        let p = |n: &str| ObjectPath::new("shop", "public", n);
        vec![
            (
                p("customers"),
                TableDetails {
                    columns: vec![col("id", "integer", true, false), col("name", "text", false, false)],
                    ..Default::default()
                },
            ),
            (
                p("orders"),
                TableDetails {
                    columns: vec![col("id", "integer", true, false), col("customer_id", "integer", false, false)],
                    foreign_keys: vec![fk("orders_customer_fk", &["customer_id"], "public", "customers", &["id"])],
                    ..Default::default()
                },
            ),
            (
                p("order_items"),
                TableDetails {
                    columns: vec![
                        col("order_id", "integer", true, false),
                        col("product_id", "integer", true, false),
                        col("qty", "numeric(10,2)", false, true),
                    ],
                    foreign_keys: vec![
                        fk("items_order_fk", &["order_id"], "public", "orders", &["id"]),
                        fk("items_product_fk", &["product_id"], "catalog", "products", &["id"]),
                    ],
                    ..Default::default()
                },
            ),
            (
                p("invoices"),
                TableDetails {
                    columns: vec![col("id", "integer", true, false), col("order_id", "integer", false, true)],
                    foreign_keys: vec![fk("invoice_order_fk", &["order_id"], "", "orders", &["id"])],
                    indexes: vec![IndexInfo {
                        name: "invoice_order_key".into(),
                        columns: vec!["order_id".into()],
                        unique: true,
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ),
            (
                p("audit_log"),
                TableDetails { columns: vec![col("at", "timestamp", false, false)], ..Default::default() },
            ),
            (
                p("employees"),
                TableDetails {
                    columns: vec![col("id", "integer", true, false), col("boss_id", "integer", false, true)],
                    foreign_keys: vec![fk("boss_fk", &["boss_id"], "public", "employees", &["id"])],
                    ..Default::default()
                },
            ),
        ]
    }

    #[test]
    fn builds_links_and_external_tables() {
        let d = Diagram::build(&shop());
        assert_eq!(d.entities.len(), 7);
        let products = d.entities.iter().position(|e| e.path.name == "products").unwrap();
        assert!(d.entities[products].external);
        assert_eq!(d.entities[products].path.schema, "catalog");
        assert_eq!(d.entities[products].columns[0].name, "id");
        let invoice = d.links.iter().find(|l| l.name == "invoice_order_fk").unwrap();
        assert_eq!(invoice.to, 1, "an empty ref schema means the same schema");
        assert!(invoice.optional);
        assert_eq!(invoice.cardinality, Cardinality::OneToOne);
        let order = d.links.iter().find(|l| l.name == "orders_customer_fk").unwrap();
        assert!(!order.optional);
        assert_eq!(order.cardinality, Cardinality::OneToMany);
        assert!(d.entities[1].columns[1].foreign_key);
        assert_eq!(d.neighbors(1), vec![0, 2, 3]);
    }

    #[test]
    fn parents_left_of_children_and_no_overlaps() {
        let d = Diagram::build(&shop());
        let sizes = estimated_sizes(&d);
        let pos = layout(&d, &sizes);
        let x = |name: &str| pos[d.entities.iter().position(|e| e.path.name == name).unwrap()].0;
        assert!(x("customers") < x("orders"));
        assert!(x("orders") < x("order_items"));
        assert!(x("orders") < x("invoices"));
        for i in 0..pos.len() {
            for j in i + 1..pos.len() {
                let (a, b) = (pos[i], pos[j]);
                let overlap = a.0 < b.0 + sizes[j].0
                    && b.0 < a.0 + sizes[i].0
                    && a.1 < b.1 + sizes[j].1
                    && b.1 < a.1 + sizes[i].1;
                assert!(!overlap, "{} overlaps {}", d.entities[i].path.name, d.entities[j].path.name);
            }
        }
        // Stable for the same input.
        assert_eq!(pos, layout(&d, &sizes));
    }

    #[test]
    fn cycles_do_not_hang() {
        let p = |n: &str| ObjectPath::new("db", "s", n);
        let t = |refs: &str| TableDetails {
            columns: vec![col("id", "int", true, false), col("r", "int", false, true)],
            foreign_keys: vec![fk("f", &["r"], "s", refs, &["id"])],
            ..Default::default()
        };
        let d = Diagram::build(&[(p("a"), t("b")), (p("b"), t("c")), (p("c"), t("a"))]);
        let pos = layout(&d, &estimated_sizes(&d));
        assert_eq!(pos.len(), 3);
    }

    #[test]
    fn mermaid_and_dot() {
        let d = Diagram::build(&shop());
        let m = mermaid(&d);
        assert!(m.starts_with("erDiagram\n    customers {\n        integer id PK\n"), "{m}");
        assert!(m.contains("        numeric_10_2 qty\n"), "{m}");
        assert!(m.contains("    customers ||--o{ orders : \"orders_customer_fk\"\n"), "{m}");
        assert!(m.contains("    orders |o--o| invoices : \"invoice_order_fk\"\n"), "{m}");
        let g = dot(&d);
        assert!(g.contains("t1:c1 -> t0:c0"), "{g}");
        assert!(g.contains("style=\"dashed\""), "external box is dashed");
        let s = svg(&d);
        assert!(s.starts_with("<svg") && s.trim_end().ends_with("</svg>"));
        assert_eq!(s.matches("<g>").count(), 7);
    }
}
