//! go-file: pkg/util/disjointset/int_set.go
//!
//! `SimpleIntSet` (block D00.3), the int disjoint set used by constant propagation.

/// SimpleIntSet is the int disjoint set.
/// It's not designed for sparse case. You should use it when the elements are continuous.
/// Time complexity: the union operation is inverse ackermann function, which is very close to O(1).
#[derive(Clone, Debug, Default)]
pub struct SimpleIntSet {
    parent: Vec<usize>,
}

/// NewIntSet returns a new int disjoint set.
// go: NewIntSet L1
pub fn new_int_set(size: usize) -> SimpleIntSet {
    SimpleIntSet {
        parent: (0..size).collect(),
    }
}

impl SimpleIntSet {
    /// Union unions two sets in int disjoint set.
    // go: SimpleIntSet.Union L1
    pub fn union(&mut self, a: usize, b: usize) {
        let ra = self.find_root(a);
        let rb = self.find_root(b);
        self.parent[ra] = rb;
    }

    /// FindRoot finds the representative element of the set that `a` belongs to.
    // go: SimpleIntSet.FindRoot L1
    pub fn find_root(&mut self, a: usize) -> usize {
        if a == self.parent[a] {
            return a;
        }
        // Path compression, which leads the time complexity to the inverse Ackermann function.
        let root = self.find_root(self.parent[a]);
        self.parent[a] = root;
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_and_find_root() {
        let mut s = new_int_set(5);
        s.union(0, 1);
        s.union(2, 3);
        s.union(1, 3);
        assert_eq!(s.find_root(0), s.find_root(2));
        assert_ne!(s.find_root(0), s.find_root(4));
        // Go: parent[FindRoot(a)] = FindRoot(b), so the root of `b` stays the root.
        let mut t = new_int_set(3);
        t.union(0, 2);
        assert_eq!(t.find_root(0), 2);
    }
}
