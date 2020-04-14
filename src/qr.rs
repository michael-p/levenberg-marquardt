//! Pivoted QR factorization and a specialized LLS solver.
//!
//! The QR factorization is used to implement an efficient solver for the
//! linear least squares problem which is repeatedly required to be
//! solved in the LM algorithm.
#[cfg(test)]
use approx::assert_relative_eq;
use nalgebra::{
    allocator::Allocator,
    constraint::{DimEq, ShapeConstraint},
    convert,
    storage::{ContiguousStorageMut, Storage},
    DefaultAllocator, Dim, DimMin, DimMinimum, Matrix, MatrixSlice, RealField, Vector, VectorN, U1,
};
use num_traits::FromPrimitive;

/// Erros which can occur using the pivoted QR factorization or the solver.
pub enum Error {
    ShapeConstraintFailed,
}

/// Pivoted QR decomposition.
///
/// Let `$\mathbf{A}\in\R^{m\times n}$` with `$m\geq n$`.
/// Then this algorithm computes a permutation matrix `$\mathbf{P}$`,
/// a matrix `$\mathbf{Q}\in\R^{m\times n}$` with orthonormal columns
/// and an upper triangular matrix `$\mathbf{R}\in\R^{n\times n}$` such that
/// ```math
/// \mathbf{P}^\top \mathbf{A} \mathbf{P} = \mathbf{Q}\mathbf{R}.
/// ```
pub struct PivotedQR<F, M, N, S>
where
    F: RealField,
    M: Dim + DimMin<N>,
    N: Dim,
    S: ContiguousStorageMut<F, M, N>,
    DefaultAllocator: Allocator<F, N> + Allocator<usize, N>,
{
    /// The column norms of the input matrix `$\mathbf{A}$`
    column_norms: VectorN<F, N>,
    /// Strictly upper part of `$\mathbf{R}$` and the Householder transformations,
    /// combined in one matrix.
    qr: Matrix<F, M, N, S>,
    /// Diagonal entries of R
    r_diag: VectorN<F, N>,
    /// Permution matrix. Entry `$i$` specifies which column of the identity
    /// matrix to use.
    permutation: VectorN<usize, N>,
    work: VectorN<F, N>,
}

impl<F, M, N, S> PivotedQR<F, M, N, S>
where
    F: RealField + FromPrimitive,
    M: Dim + DimMin<N>,
    N: Dim,
    S: ContiguousStorageMut<F, M, N>,
    DefaultAllocator: Allocator<F, N> + Allocator<F, N> + Allocator<usize, N>,
{
    /// Create a pivoted QR decomposition of a matrix `$\mathbf{A}\in\R^{m\times n}$`
    /// with `$m \geq n$`.
    ///
    /// # Errors
    ///
    /// Only returns `Err` when `$m < n$`.
    pub fn new(mut a: Matrix<F, M, N, S>) -> Result<Self, Error> {
        // The implementation is based more or less on LAPACK's "xGEQPF"
        let n = a.data.shape().1;
        if a.nrows() < n.value() {
            return Err(Error::ShapeConstraintFailed);
        }
        let column_norms =
            VectorN::<F, N>::from_iterator_generic(n, U1, a.column_iter().map(|c| c.norm()));
        let mut r_diag = column_norms.clone();
        let mut work = column_norms.clone();
        let mut permutation = VectorN::<usize, N>::from_iterator_generic(n, U1, 0..);
        for j in 0..n.value() {
            // pivot
            {
                let kmax = r_diag.slice_range(j.., ..).imax() + j;
                a.swap_columns(j, kmax);
                permutation.swap_rows(j, kmax);
                r_diag[kmax] = r_diag[j];
                work[kmax] = work[j];
            }
            // compute Householder reflection vector w_j to
            // reduce the j-th column
            let mut lower = a.rows_range_mut(j..);
            let (left, mut right) = lower.columns_range_pair_mut(j, j + 1..);
            let w_j = {
                let mut axis = left;
                let mut aj_norm = axis.norm();
                if aj_norm.is_zero() {
                    r_diag[j] = F::zero();
                    continue;
                }
                if axis[0].is_negative() {
                    aj_norm = -aj_norm;
                }
                r_diag[j] = -aj_norm;
                axis.unscale_mut(aj_norm);
                axis[0] += F::one();
                axis
            };
            // apply reflection to remaining rows
            for (mut k, mut col) in right.column_iter_mut().enumerate() {
                let temp = {
                    let sum = col.dot(&w_j);
                    sum / w_j[0]
                };
                col.axpy(-temp, &w_j, F::one());
                // update partial column norms
                // see "Lapack Working Note 176"
                k += j + 1;
                if r_diag[k].is_zero() {
                    continue;
                }
                let temp = col[0] / r_diag[k];
                let temp = if temp.abs() < F::one() {
                    r_diag[k] *= (F::one() - temp * temp).sqrt();
                    r_diag[k] / work[k]
                } else {
                    F::zero()
                };
                let z005: F = convert(0.05f64);
                if temp.abs().is_zero() || z005 * (temp * temp) <= F::default_epsilon() {
                    r_diag[k] = col.slice_range(1.., ..).norm();
                    work[k] = r_diag[k];
                }
            }
        }
        Ok(Self {
            column_norms,
            qr: a,
            permutation,
            r_diag,
            work,
        })
    }

    /// Consume the QR-decomposition and transform it into
    /// a parametrized least squares problem.
    ///
    /// See [`LinearLeastSquaresDiagonalProblem`](struct.LinearLeastSquaresDiagonalProblem.html)
    /// for details.
    pub fn into_least_squares_diagonal_problem<QS>(
        mut self,
        mut b: Vector<F, M, QS>,
    ) -> LinearLeastSquaresDiagonalProblem<F, M, N, S>
    where
        QS: ContiguousStorageMut<F, M>,
        ShapeConstraint: DimEq<DimMinimum<M, N>, N>,
    {
        // compute first n-entries of Q^T * b
        let n = self.qr.data.shape().1;
        let mut qt_b = VectorN::<F, N>::from_column_slice_generic(n, U1, b.as_slice());
        for j in 0..n.value() {
            let axis = self.qr.slice_range(j.., j);
            if !axis[0].is_zero() {
                let temp = b.rows_range(j..).dot(&axis) / axis[0];
                b.rows_range_mut(j..).axpy(-temp, &axis, F::one());
            }
            qt_b[j] = b[j];
        }
        self.qr.set_diagonal(&self.r_diag);
        LinearLeastSquaresDiagonalProblem {
            qt_b,
            column_norms: self.column_norms,
            upper_r: self.qr,
            l_diag: self.r_diag,
            permutation: self.permutation,
            work: self.work,
        }
    }
}

/// Parametrized linear least squares problem for the LM algorithm.
///
/// The problem is of the form
/// ```math
///   \min_{\vec{x}\in\R^n}\frac{1}{2}\Bigl\|
///     \begin{bmatrix}
///        \mathbf{A} \\
///        \mathbf{D}
///     \end{bmatrix}\vec{x} -
///     \begin{bmatrix}
///         \vec{b} \\
///         \vec{0}
///     \end{bmatrix}
///   \Bigr\|^2,
/// ```
/// for a matrix `$\mathbf{A}\in\R^{m \times n}$`, diagonal matrix
/// `$\mathbf{D}\in\R^n$` and vector `$\vec{b}\in\R^m$`.
/// Everything except the diagonal matrix `$\mathbf{D}$` is considered
/// fixed.
///
/// The problem can be efficiently solved for a sequence of diagonal
/// matrices `$\mathbf{D}$`.
///
/// You must create an instance of this by first computing a pivotized
/// QR decomposition of `$\mathbf{A}$`, then use
/// [`into_least_squares_diagonal_problem`](struct.PivotedQR.html#into_least_squares_diagonal_problem).
pub struct LinearLeastSquaresDiagonalProblem<F, M, N, S>
where
    F: RealField,
    M: Dim,
    N: Dim,
    S: ContiguousStorageMut<F, M, N>,
    DefaultAllocator: Allocator<F, N> + Allocator<usize, N>,
{
    /// The first `$n$` entries of `$\mathbf{Q}^\top \vec{b}$`.
    qt_b: VectorN<F, N>,
    /// Upper part of `$\mathbf{R}$`, also used to store strictly lower part of `$\mathbf{L}$`.
    upper_r: Matrix<F, M, N, S>,
    /// Diagonal entries of `$\mathbf{L}$`.
    l_diag: VectorN<F, N>,
    /// Permution matrix. Entry `$i$` specifies which column of the identity
    /// matrix to use.
    permutation: VectorN<usize, N>,
    pub(crate) column_norms: VectorN<F, N>,
    work: VectorN<F, N>,
}

pub struct CholeskyFactor<'a, F, M, N, S>
where
    F: RealField,
    M: Dim,
    N: Dim,
    S: ContiguousStorageMut<F, M, N>,
    DefaultAllocator: Allocator<F, N> + Allocator<usize, N>,
{
    pub permutation: &'a VectorN<usize, N>,
    l: MatrixSlice<'a, F, N, N, S::RStride, S::CStride>,
    work: &'a mut VectorN<F, N>,
    qt_b: &'a VectorN<F, N>,
    lower: bool,
    l_diag: &'a VectorN<F, N>,
}

impl<'a, F, M, N, S> CholeskyFactor<'a, F, M, N, S>
where
    F: RealField,
    M: Dim,
    N: Dim,
    S: ContiguousStorageMut<F, M, N>,
    DefaultAllocator: Allocator<F, N> + Allocator<usize, N>,
{
    /// Solve the equation `$\mathbf{L}\vec{x} = \mathbf{P}^\top \vec{b}$`.
    pub fn solve(&mut self, mut rhs: VectorN<F, N>) -> VectorN<F, N> {
        for i in 0..self.work.nrows() {
            self.work[i] = rhs[self.permutation[i]];
        }
        if self.lower {
            let n = self.work.nrows();
            for j in 0..n {
                let x = unsafe {
                    let x = self.work.vget_unchecked_mut(j);
                    *x /= *self.l_diag.vget_unchecked(j);
                    *x
                };
                self.work.slice_range_mut(j + 1.., 0).axpy(
                    -x,
                    &self.l.slice_range(j + 1.., j),
                    F::one(),
                );
            }
        } else {
            self.l.tr_solve_upper_triangular_mut(self.work);
        }
        core::mem::swap(self.work, &mut rhs);
        rhs
    }

    /// Computes `$\mathbf{L}\mathbf{Q}^\top\vec{b}$`.
    pub fn mul_qt_b(&mut self, mut out: VectorN<F, N>) -> VectorN<F, N> {
        out.fill(F::zero());
        if self.lower {
            for (i, col) in self.l.column_iter().enumerate() {
                out.rows_range_mut(i + 1..)
                    .axpy(self.qt_b[i], &col.rows_range(i + 1..), F::one());
                out[i] += self.qt_b[i] * self.l_diag[i];
            }
        } else {
            for (i, col) in self.l.column_iter().enumerate() {
                out[i] = self.qt_b.rows_range(..i + 1).dot(&col.rows_range(..i + 1));
            }
        }
        out
    }
}

impl<F, M, N, S> LinearLeastSquaresDiagonalProblem<F, M, N, S>
where
    F: RealField,
    M: Dim,
    N: Dim,
    S: ContiguousStorageMut<F, M, N>,
    DefaultAllocator: Allocator<F, N> + Allocator<usize, N>,
{
    /// Compute scaled maximum of dot products between `$\vec{b}$` and the columns of `$\mathbf{A}$`.
    ///
    /// It computes
    /// ```math
    ///   \max_{i=1,\ldots,n}\frac{|(\mathbf{A}^\top \vec{b})_i|}{\|\mathbf{A}\vec{e}_i\|}.
    /// ```
    ///
    /// A fraction with column norm zero is counted as zero. If any
    /// of the computations are nan, `None` is returned.
    pub fn max_a_t_b_scaled(&self) -> Option<F> {
        // compute max column of Ab scaled by column norm of A
        let mut max = F::zero();
        #[allow(clippy::eq_op)]
        for (j, col) in self.upper_r.column_iter().enumerate() {
            let scale = self.column_norms[self.permutation[j]];
            if scale.is_zero() {
                continue;
            }
            let sum = col.rows_range(..j + 1).dot(&self.qt_b.rows_range(..j + 1));
            if sum != sum || scale != scale {
                return None;
            }
            max = max.max(sum.abs() / scale);
        }
        Some(max)
    }

    /// Compute `$\|\mathbf{A}\vec{x}\|^2 = \vec{x}^\top\mathbf{A}^\top\mathbf{A}\vec{x}$`.
    pub fn a_x_norm_squared(&mut self, x: &VectorN<F, N>) -> F {
        self.work.fill(F::zero());
        for (i, (col, idx)) in self
            .upper_r
            .column_iter()
            .zip(self.permutation.iter())
            .enumerate()
        {
            self.work
                .rows_range_mut(..i + 1)
                .axpy(x[*idx], &col.rows_range(..i + 1), F::one());
        }
        self.work.norm_squared()
    }

    /// Solve the linear least squares problem
    /// for a diagonal matrix `$\mathbf{D}$` (`diag`).
    ///
    /// This is equivalent to solving
    /// ```math
    /// (\mathbf{A}^\top\mathbf{A} + \mathbf{D}\mathbf{D})\vec{x} = \mathbf{A}^\top \vec{b}.
    /// ```
    ///
    /// # Return value
    ///
    /// Returns the solution `$\vec{x}$` and a reference to a lower triangular
    /// matrix `$\mathbf{L}\in\R^{n\times n}$` with
    /// ```math
    ///   \mathbf{P}^\top(\mathbf{A}^\top\mathbf{A} + \mathbf{D}\mathbf{D})\mathbf{P} = \mathbf{L}\mathbf{L}^\top.
    /// ```
    pub fn solve_with_diagonal(
        &mut self,
        diag: &VectorN<F, N>,
        mut out: VectorN<F, N>,
    ) -> (VectorN<F, N>, CholeskyFactor<F, M, N, S>) {
        out.copy_from(&self.qt_b);
        let mut rhs = self.eliminate_diag(diag, out /* rhs */);
        core::mem::swap(&mut self.work, &mut rhs);
        self.solve_after_elimination(rhs)
    }

    /// Solve the least squares problem with a zero diagonal.
    pub fn solve_with_zero_diagonal(&mut self) -> (VectorN<F, N>, CholeskyFactor<F, M, N, S>) {
        let n = self.upper_r.data.shape().1;
        let l = self.upper_r.generic_slice((0, 0), (n, n));
        self.work.copy_from(&self.qt_b);
        l.solve_upper_triangular_mut(&mut self.work);
        let x = VectorN::<F, N>::from_iterator_generic(
            n,
            U1,
            (0..n.value()).map(|j| self.work[self.permutation[j]]),
        );
        let chol = CholeskyFactor {
            permutation: &self.permutation,
            l,
            work: &mut self.work,
            qt_b: &self.qt_b,
            lower: false,
            l_diag: &self.l_diag,
        };
        (x, chol)
    }

    pub fn has_full_rank(&self) -> bool {
        let n = self.upper_r.ncols();
        !(0..n).any(|j| self.upper_r[(j, j)].is_zero())
    }

    fn rank(&self) -> usize {
        self.l_diag
            .iter()
            .position(F::is_zero)
            .unwrap_or_else(|| self.l_diag.nrows())
    }

    fn solve_after_elimination(
        &mut self,
        mut x: VectorN<F, N>,
    ) -> (VectorN<F, N>, CholeskyFactor<F, M, N, S>) {
        let rank = self.rank();
        let rhs = &mut self.work;
        rhs.rows_range_mut(rank..).fill(F::zero());

        let n = self.upper_r.data.shape().1;
        let l = self.upper_r.generic_slice((0, 0), (n, n));

        // solve L^T * x = rhs
        for j in (0..rank).rev() {
            let dot = l
                .slice_range(j + 1..rank, j)
                .dot(&rhs.slice_range(j + 1..rank, 0));
            unsafe {
                let x = rhs.vget_unchecked_mut(j);
                let diag = self.l_diag.vget_unchecked(j);
                *x = (*x - dot) / *diag;
            }
        }

        for j in 0..n.value() {
            x[self.permutation[j]] = rhs[j];
        }
        let cholesky_factor = CholeskyFactor {
            l,
            work: &mut self.work,
            permutation: &self.permutation,
            qt_b: &self.qt_b,
            lower: true,
            l_diag: &self.l_diag,
        };
        (x, cholesky_factor)
    }

    fn eliminate_diag<DS>(
        &mut self,
        diag: &Vector<F, N, DS>,
        mut rhs: VectorN<F, N>,
    ) -> VectorN<F, N>
    where
        DS: Storage<F, N>,
    {
        // only lower triangular part is used which was filled with R^T by
        // `copy_r_down`. This part is then iteratively overwritten with L.
        let n = self.upper_r.data.shape().1;
        let mut r_and_l = self.upper_r.generic_slice_mut((0, 0), (n, n));
        r_and_l.fill_lower_triangle_with_upper_triangle();
        let n = diag.nrows();
        for j in 0..n {
            // Safe diagonal of R.
            unsafe {
                *self.work.vget_unchecked_mut(j) = *r_and_l.get_unchecked((j, j));
            };
        }
        // eliminate the diagonal entries from D using Givens rotations
        for j in 0..n {
            let diag_entry = unsafe { *diag.vget_unchecked(*self.permutation.vget_unchecked(j)) };
            if !diag_entry.is_zero() {
                self.l_diag[j] = diag_entry;
                self.l_diag.rows_range_mut(j + 1..).fill(F::zero());

                let mut qtbpj = F::zero();
                for k in j..n {
                    if self.l_diag[k].is_zero() {
                        continue;
                    }
                    let r_kk = unsafe { r_and_l.get_unchecked_mut((k, k)) };
                    // determine the Givens rotation
                    let (sin, cos) = if r_kk.abs() < self.l_diag[k].abs() {
                        let cot = *r_kk / self.l_diag[k];
                        let sin = (F::one() + cot * cot).sqrt().recip();
                        (sin, sin * cot)
                    } else {
                        let tan = self.l_diag[k] / (*r_kk);
                        let cos = (F::one() + tan * tan).sqrt().recip();
                        (cos * tan, cos)
                    };
                    // compute the modified diagonal element of R and (Q^T*b,0)
                    *r_kk = cos * (*r_kk) + sin * self.l_diag[k];
                    let rhs_k = unsafe { rhs.vget_unchecked_mut(k) };
                    let temp = cos * (*rhs_k) + sin * qtbpj;
                    qtbpj = -sin * (*rhs_k) + cos * qtbpj;
                    *rhs_k = temp;

                    // accumulate the transformation in the row of L
                    for i in k + 1..n {
                        let r_ik = unsafe { r_and_l.get_unchecked_mut((i, k)) };
                        let temp = cos * (*r_ik) + sin * self.l_diag[i];
                        self.l_diag[i] = -sin * (*r_ik) + cos * self.l_diag[i];
                        *r_ik = temp;
                    }
                }
            }
            self.l_diag[j] = r_and_l[(j, j)];
            r_and_l[(j, j)] = unsafe { *self.work.vget_unchecked(j) };
        }
        rhs
    }
}

#[test]
fn test_pivoted_qr() {
    // Reference data was generated using the implementation from the library
    // "lmfit".
    // Also, the values were checked with SciPy's "qr" method.
    use nalgebra::{Matrix4x3, Vector3};
    let a = Matrix4x3::<f64>::from_iterator((0..).map(|i| i as f64));
    let qr = PivotedQR::new(a).ok().unwrap();

    assert_eq!(qr.permutation, nalgebra::Vector3::new(2, 0, 1));

    let column_norms = Vector3::new(3.7416574, 11.2249722, 19.1311265);
    assert_relative_eq!(qr.column_norms, column_norms, epsilon = 1e-7);

    let r_diag = Vector3::new(-19.1311265, 1.8700983, 0.0);
    assert_relative_eq!(qr.r_diag, r_diag, epsilon = 1e-7);

    let qr_ref = Matrix4x3::<f64>::from_iterator(
        [
            1.4181667,
            0.4704375,
            0.5227084,
            0.5749792,
            -3.2407919,
            1.0401278,
            -0.4307302,
            -0.9015882,
            -11.1859592,
            0.9350492,
            1.7310553,
            0.6823183,
        ]
        .iter()
        .map(|x| *x),
    );
    assert_relative_eq!(qr.qr, qr_ref, epsilon = 1e-7);
}

#[test]
fn test_pivoted_qr_more_branches() {
    // This test case was crafted to hit all three
    // branches of the partial column norms
    use nalgebra::{Matrix4x3, Vector3};
    let a = Matrix4x3::<f64>::from_iterator(
        [
            30.0, 43.0, 34.0, 26.0, 30.0, 43.0, 34.0, 26.0, 24.0, 39.0, -10.0, -34.0,
        ]
        .iter()
        .map(|x| *x),
    );
    let qr = PivotedQR::new(a).ok().unwrap();
    let r_diag = Vector3::new(-67.683085036070864, -55.250741178610944, 0.00000000000001);
    assert_relative_eq!(qr.r_diag, r_diag);
}

#[cfg(test)]
fn default_lls(
    case: usize,
) -> LinearLeastSquaresDiagonalProblem<
    f64,
    nalgebra::U4,
    nalgebra::U3,
    nalgebra::storage::Owned<f64, nalgebra::U4, nalgebra::U3>,
> {
    use nalgebra::{Matrix4x3, Vector4};
    let a = match case {
        1 => Matrix4x3::<f64>::from_iterator((0..).map(|i| i as f64)),
        2 => Matrix4x3::<f64>::from_iterator(
            [30., 43., 34., 26., 30., 43., 34., 26., 24., 39., -10., -34.]
                .iter()
                .map(|x| *x),
        ),
        3 => Matrix4x3::new(1., 2., -1., 0., 1., 4., 0., 0., 0.5, 0., 0., 0.),
        _ => unimplemented!(),
    };
    let qr = PivotedQR::new(a).ok().unwrap();
    qr.into_least_squares_diagonal_problem(Vector4::new(1.0, 2.0, 5.0, 4.0))
}

#[test]
fn test_into_lls() {
    use nalgebra::Vector3;
    let lls = default_lls(1);
    let qt_b = Vector3::new(-6.272500481871799, 1.963603245291175, -0.288494026015405);
    assert_relative_eq!(lls.qt_b, qt_b, epsilon = 1e-14);
}

#[test]
fn test_elimate_diag_and_l() {
    use nalgebra::{Matrix3, Vector3};
    let mut lls = default_lls(1);
    let rhs = lls.eliminate_diag(&Vector3::new(1.0, 0.5, 0.0), lls.qt_b.clone());
    let rhs_ref = Vector3::new(-6.272500481871799, 1.731584982206922, 0.612416936078506);
    assert_relative_eq!(rhs, rhs_ref);

    // contains L
    let ldiag_ref = Vector3::new(-19.131126469708992, 2.120676250530203, 0.666641352293790);
    assert_relative_eq!(lls.l_diag, ldiag_ref);

    let r_ref = Matrix3::new(
        -19.131126469708992,
        -3.240791915633763,
        -11.185959192671376,
        -3.240791915633763,
        1.870098328848738,
        0.935049164424371,
        -11.185959192671376,
        0.824564277241393,
        -0.000000000000001,
    );
    let r = Matrix3::from_iterator(lls.upper_r.slice_range(..3, ..3).iter().map(|x| *x));
    assert_relative_eq!(r, r_ref);
}

#[test]
fn test_lls_x_1() {
    use nalgebra::Vector3;
    let mut lls = default_lls(1);
    let (x_out, _) = lls.solve_with_diagonal(&Vector3::new(1.0, 0.5, 0.0), Vector3::zeros());
    let x_ref = Vector3::new(0.459330143540669, 0.918660287081341, -0.287081339712919);
    assert_relative_eq!(x_out, x_ref, epsilon = 1e-14);
}

#[test]
fn test_lls_x_2() {
    // R is singular but L is not
    use nalgebra::*;
    let a = Matrix4x3::from_column_slice(&[
        14., -12., 20., -11., 19., 38., -4., -11., -14., 12., -20., 11.,
    ]);
    let qr = PivotedQR::new(a).ok().unwrap();
    let mut lls = qr.into_least_squares_diagonal_problem(Vector4::new(-5., 3., -2., 7.));

    let rdiag_exp = Vector3::new(-44.068129073061407, 29.147349299100057, 0.);
    let rdiag_out = Vector3::from_iterator(
        lls.upper_r
            .slice_range(..3, ..3)
            .diagonal()
            .iter()
            .map(|x| *x),
    );
    assert_relative_eq!(rdiag_out, rdiag_exp);

    let diag = Vector3::new(2.772724292099739, 0.536656314599949, 0.089442719099992);
    let (x_out, _) = lls.solve_with_diagonal(&diag, Vector3::zeros());
    let x_exp = Vector3::new(-0.000277544878320, -0.046225239392197, 0.266720628065249);
    assert_relative_eq!(x_out, x_exp, epsilon = 1e-14);
}

#[test]
fn test_lls_zero_diagonal() {
    use nalgebra::Vector3;
    let mut lls = default_lls(3);
    assert!(lls.has_full_rank());
    let (x_out, _l) = lls.solve_with_zero_diagonal();
    let x_ref = Vector3::new(87., -38., 10.);
    assert_relative_eq!(x_out, x_ref);
}

#[test]
fn test_cholesky_lower() {
    use nalgebra::{storage::Owned, Matrix3, Vector3, U3};
    let l = Matrix3::new(-1.0e10, 100., -1., 1., 1.0e8, 0.5, 1., 0.5, 100.);
    let slice = l.slice_range(.., ..);
    let mut chol = CholeskyFactor::<f64, U3, _, Owned<f64, U3, U3>> {
        l: slice,
        l_diag: &Vector3::new(2., 1.5, 0.1),
        lower: true,
        work: &mut Vector3::zeros(),
        permutation: &Vector3::<usize>::new(1, 0, 2),
        qt_b: &Vector3::new(1.0, 2.0, 0.5),
    };

    let out_mul = chol.mul_qt_b(Vector3::zeros());
    let exp_mul = Vector3::new(2., 4., 2.05);
    assert_relative_eq!(out_mul, exp_mul);

    let out_solve = chol.solve(Vector3::new(1.0, 2.0, 0.5));
    let exp_solve = Vector3::new(1., 0., -5.);
    assert_relative_eq!(out_solve, exp_solve);
}

#[test]
fn test_cholesky_upper() {
    use nalgebra::{storage::Owned, Matrix3, Vector3, U3};
    let l = Matrix3::new(4., 7., 1., 123., 6., 8., 34., 34455., 9.);
    let slice = l.slice_range(.., ..);
    let mut chol = CholeskyFactor::<f64, U3, _, Owned<f64, U3, U3>> {
        l: slice,
        l_diag: &Vector3::new(1234.0, -1.5, -1e120),
        lower: false,
        work: &mut Vector3::zeros(),
        permutation: &Vector3::<usize>::new(2, 1, 0),
        qt_b: &Vector3::new(1.0, 2.0, 0.5),
    };

    let out_mul = chol.mul_qt_b(Vector3::zeros());
    let exp_mul = Vector3::new(4., 19., 21.5);
    assert_relative_eq!(out_mul, exp_mul);

    let out_solve = chol.solve(Vector3::new(1.0, 2.0, 0.5));
    let exp_solve = Vector3::new(0.125, 0.1875, -0.06944444444444445);
    assert_relative_eq!(out_solve, exp_solve);
}

#[test]
fn test_column_max_norm() {
    use ::core::f64::NAN;
    use nalgebra::*;
    let a = Matrix4x3::from_column_slice(&[
        14., -12., 20., -11., 19., 38., -4., -11., -14., 12., -20., 11.,
    ]);
    let qr = PivotedQR::new(a).ok().unwrap();
    let b = Vector4::new(1., 2., 3., 4.);
    let max_at_b = qr.into_least_squares_diagonal_problem(b).max_a_t_b_scaled();
    assert_relative_eq!(max_at_b.unwrap(), 0.88499332, epsilon = 1e-8);

    let a = Matrix4x3::from_column_slice(&[
        NAN, -12., 20., -11., 19., 38., -4., -11., -14., 12., -20., 11.,
    ]);
    let qr = PivotedQR::new(a).ok().unwrap();
    let b = Vector4::new(1., 2., 3., 4.);
    let max_at_b = qr.into_least_squares_diagonal_problem(b).max_a_t_b_scaled();
    assert_eq!(max_at_b, None);

    let a = Matrix4x3::zeros();
    let qr = PivotedQR::new(a).ok().unwrap();
    let b = Vector4::new(1., 2., 3., 4.);
    let max_at_b = qr.into_least_squares_diagonal_problem(b).max_a_t_b_scaled();
    assert_eq!(max_at_b, Some(0.));
}

#[test]
fn test_a_x_norm_squared() {
    use nalgebra::*;
    let a = Matrix4x3::new(3., 6., 2., 7., 4., 3., 2., 0., 4., 5., 1., 6.);
    let qr = PivotedQR::new(a).ok().unwrap();
    let mut lls = qr.into_least_squares_diagonal_problem(Vector4::zeros());
    let result = lls.a_x_norm_squared(&Vector3::new(1., 8., 3.));
    assert_relative_eq!(result, 6710., epsilon = 1e-11);
}
