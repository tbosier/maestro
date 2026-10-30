//! Split layout as a tree of views. Pure geometry: the terminal front end
//! asks for rectangles and draws into them.

use crate::editor::ViewId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> u16 {
        self.x.saturating_add(self.width)
    }

    pub fn bottom(&self) -> u16 {
        self.y.saturating_add(self.height)
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// `Vertical` places children side by side (a vertical divider between
/// them); `Horizontal` stacks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Leaf(ViewId),
    Split { dir: Direction, children: Vec<Node> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: Node,
}

/// A placed view plus where to draw the divider to its right, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub view: ViewId,
    pub rect: Rect,
}

impl Layout {
    pub fn new(view: ViewId) -> Self {
        Self {
            root: Node::Leaf(view),
        }
    }

    pub fn views(&self) -> Vec<ViewId> {
        fn walk(n: &Node, out: &mut Vec<ViewId>) {
            match n {
                Node::Leaf(v) => out.push(*v),
                Node::Split { children, .. } => children.iter().for_each(|c| walk(c, out)),
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &mut out);
        out
    }

    /// Splits `target`, placing `new` after it.
    pub fn split(&mut self, target: ViewId, new: ViewId, dir: Direction) {
        fn walk(n: &mut Node, target: ViewId, new: ViewId, dir: Direction) -> bool {
            match n {
                Node::Leaf(v) if *v == target => {
                    *n = Node::Split {
                        dir,
                        children: vec![Node::Leaf(target), Node::Leaf(new)],
                    };
                    true
                }
                Node::Leaf(_) => false,
                Node::Split { dir: d, children } => {
                    if *d == dir
                        && let Some(i) = children
                            .iter()
                            .position(|c| matches!(c, Node::Leaf(v) if *v == target))
                    {
                        children.insert(i + 1, Node::Leaf(new));
                        return true;
                    }
                    children.iter_mut().any(|c| walk(c, target, new, dir))
                }
            }
        }
        walk(&mut self.root, target, new, dir);
    }

    /// Removes a view. Returns false if it is the last one.
    pub fn remove(&mut self, target: ViewId) -> bool {
        if self.root == Node::Leaf(target) {
            return false;
        }
        fn walk(n: &mut Node, target: ViewId) -> bool {
            let Node::Split { children, .. } = n else {
                return false;
            };
            if let Some(i) = children
                .iter()
                .position(|c| matches!(c, Node::Leaf(v) if *v == target))
            {
                children.remove(i);
                if children.len() == 1 {
                    *n = children.pop().expect("one child left");
                }
                return true;
            }
            children.iter_mut().any(|c| walk(c, target))
        }
        walk(&mut self.root, target)
    }

    /// Keeps only `target`.
    pub fn only(&mut self, target: ViewId) {
        self.root = Node::Leaf(target);
    }

    /// Computes view rectangles. Side-by-side views leave a one-column gap
    /// for a divider between them.
    pub fn compute(&self, area: Rect) -> Vec<Placement> {
        fn walk(n: &Node, area: Rect, out: &mut Vec<Placement>) {
            match n {
                Node::Leaf(v) => out.push(Placement {
                    view: *v,
                    rect: area,
                }),
                Node::Split { dir, children } => {
                    let count = children.len() as u16;
                    match dir {
                        Direction::Vertical => {
                            let gaps = count.saturating_sub(1);
                            let usable = area.width.saturating_sub(gaps);
                            let mut x = area.x;
                            for (i, c) in children.iter().enumerate() {
                                let i = i as u16;
                                let w = usable / count + u16::from(i < usable % count);
                                walk(c, Rect::new(x, area.y, w, area.height), out);
                                x = x.saturating_add(w + 1);
                            }
                        }
                        Direction::Horizontal => {
                            let mut y = area.y;
                            for (i, c) in children.iter().enumerate() {
                                let i = i as u16;
                                let h = area.height / count + u16::from(i < area.height % count);
                                walk(c, Rect::new(area.x, y, area.width, h), out);
                                y = y.saturating_add(h);
                            }
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, area, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_compute_remove() {
        let (a, b, c) = (ViewId(1), ViewId(2), ViewId(3));
        let mut l = Layout::new(a);
        l.split(a, b, Direction::Vertical);
        l.split(b, c, Direction::Horizontal);
        let p = l.compute(Rect::new(0, 0, 81, 40));
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].rect, Rect::new(0, 0, 40, 40));
        assert_eq!(p[1].rect, Rect::new(41, 0, 40, 20));
        assert_eq!(p[2].rect, Rect::new(41, 20, 40, 20));
        assert!(l.remove(b));
        assert_eq!(l.views(), vec![a, c]);
        assert!(l.remove(c));
        assert!(!l.remove(a));
    }
}
