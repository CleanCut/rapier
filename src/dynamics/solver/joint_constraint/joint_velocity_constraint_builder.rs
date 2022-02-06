use crate::dynamics::solver::joint_constraint::joint_velocity_constraint::{
    JointVelocityConstraint, JointVelocityGroundConstraint, WritebackId,
};
use crate::dynamics::solver::joint_constraint::SolverBody;
use crate::dynamics::solver::MotorParameters;
use crate::dynamics::{IntegrationParameters, JointIndex};
use crate::math::{Isometry, Matrix, Point, Real, Rotation, Vector, ANG_DIM, DIM};
use crate::utils::{IndexMut2, WCrossMatrix, WDot, WQuat, WReal};
use na::{SMatrix, UnitQuaternion};
use simba::simd::{SimdRealField, SimdValue};

#[derive(Debug, Copy, Clone)]
pub struct JointVelocityConstraintBuilder<N: SimdRealField> {
    pub basis: Matrix<N>,
    pub cmat1_basis: SMatrix<N, ANG_DIM, DIM>,
    pub cmat2_basis: SMatrix<N, ANG_DIM, DIM>,
    pub lin_err: Vector<N>,
    pub ang_err: Vector<N>,
}

impl<N: WReal> JointVelocityConstraintBuilder<N> {
    pub fn new(
        frame1: &Isometry<N>,
        frame2: &Isometry<N>,
        world_com1: &Point<N>,
        world_com2: &Point<N>,
        locked_lin_axes: u8,
    ) -> Self {
        let mut frame1 = *frame1;
        let mut basis = frame1.rotation.to_rotation_matrix().into_inner();

        #[cfg(feature = "dim3")]
        {
            let sgn = N::one().simd_copysign(frame1.rotation.dot(&frame2.rotation));
            basis *= sgn;
        }

        let lin_err = frame2.translation.vector - frame1.translation.vector;

        // Adjust the point of application of the force for the first body,
        // by snapping free axes to the second frame’s center (to account for
        // the allowed relative movement).
        {
            let mut new_center1 = frame2.translation.vector; // First, assume all dofs are free.

            // Then snap the locked ones.
            for i in 0..DIM {
                if locked_lin_axes & (1 << i) != 0 {
                    let axis = basis.column(i);
                    new_center1 -= axis * lin_err.dot(&axis);
                }
            }
            frame1.translation.vector = new_center1;
        }

        let r1 = frame1.translation.vector - world_com1.coords;
        let r2 = frame2.translation.vector - world_com2.coords;

        let cmat1 = r1.gcross_matrix();
        let cmat2 = r2.gcross_matrix();

        // TODO: move this to nalgebra
        fn scaled_axis<N: WReal>(quat: UnitQuaternion<N>) -> Vector<N> {
            let angle = quat.angle();
            let v = quat.as_ref().vector() * quat.quaternion().scalar().simd_signum();
            let v_norm = v.norm();
            let axis = v * crate::utils::simd_inv(v_norm);
            axis * angle
        }

        let mut ang_err = scaled_axis(frame1.rotation.inverse() * frame2.rotation);

        Self {
            basis,
            cmat1_basis: cmat1 * basis,
            cmat2_basis: cmat2 * basis,
            lin_err,
            ang_err: na::zero(),
        }
    }

    pub fn limit_linear<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        limited_axis: usize,
        limits: [N; 2],
        writeback_id: WritebackId,
    ) -> JointVelocityConstraint<N, LANES> {
        let zero = N::zero();
        let mut constraint =
            self.lock_linear(params, joint_id, body1, body2, limited_axis, writeback_id);

        let dist = self.lin_err.dot(&constraint.lin_jac);
        let min_enabled = dist.simd_lt(limits[0]);
        let max_enabled = limits[1].simd_lt(dist);

        let erp_inv_dt = N::splat(params.joint_erp_inv_dt());
        let cfm_coeff = N::splat(params.joint_cfm_coeff());
        let rhs_bias =
            ((dist - limits[1]).simd_max(zero) - (limits[0] - dist).simd_max(zero)) * erp_inv_dt;
        constraint.rhs = constraint.rhs_wo_bias + rhs_bias;
        constraint.cfm_coeff = cfm_coeff;
        constraint.impulse_bounds = [
            N::splat(-Real::INFINITY).select(min_enabled, zero),
            N::splat(Real::INFINITY).select(max_enabled, zero),
        ];

        constraint
    }

    pub fn motor_linear<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        motor_axis: usize,
        motor_params: &MotorParameters<N>,
        limits: Option<[N; 2]>,
        writeback_id: WritebackId,
    ) -> JointVelocityConstraint<N, LANES> {
        let inv_dt = N::splat(params.inv_dt());
        let mut constraint =
            self.lock_linear(params, joint_id, body1, body2, motor_axis, writeback_id);

        let mut rhs_wo_bias = N::zero();
        if motor_params.erp_inv_dt != N::zero() {
            let dist = self.lin_err.dot(&constraint.lin_jac);
            rhs_wo_bias += (dist - motor_params.target_pos) * motor_params.erp_inv_dt;
        }

        let mut target_vel = motor_params.target_vel;
        if let Some(limits) = limits {
            let dist = self.lin_err.dot(&constraint.lin_jac);
            target_vel =
                target_vel.simd_clamp((limits[0] - dist) * inv_dt, (limits[1] - dist) * inv_dt);
        };

        let dvel = constraint.lin_jac.dot(&(body2.linvel - body1.linvel))
            + (constraint.ang_jac2.gdot(body2.angvel) - constraint.ang_jac1.gdot(body1.angvel));
        rhs_wo_bias += dvel - target_vel;

        constraint.cfm_coeff = motor_params.cfm_coeff;
        constraint.cfm_gain = motor_params.cfm_gain;
        constraint.impulse_bounds = [-motor_params.max_impulse, motor_params.max_impulse];
        constraint.rhs = rhs_wo_bias;
        constraint.rhs_wo_bias = rhs_wo_bias;
        constraint
    }

    pub fn lock_linear<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        locked_axis: usize,
        writeback_id: WritebackId,
    ) -> JointVelocityConstraint<N, LANES> {
        let lin_jac = self.basis.column(locked_axis).into_owned();
        #[cfg(feature = "dim2")]
        let mut ang_jac1 = self.cmat1_basis[locked_axis];
        #[cfg(feature = "dim2")]
        let mut ang_jac2 = self.cmat2_basis[locked_axis];
        #[cfg(feature = "dim3")]
        let mut ang_jac1 = self.cmat1_basis.column(locked_axis).into_owned();
        #[cfg(feature = "dim3")]
        let mut ang_jac2 = self.cmat2_basis.column(locked_axis).into_owned();

        let dvel = lin_jac.dot(&(body2.linvel - body1.linvel))
            + (ang_jac2.gdot(body2.angvel) - ang_jac1.gdot(body1.angvel));
        let rhs_wo_bias = dvel;

        let erp_inv_dt = N::splat(params.joint_erp_inv_dt());
        let cfm_coeff = N::splat(params.joint_cfm_coeff());
        let rhs_bias = lin_jac.dot(&self.lin_err) * erp_inv_dt;

        ang_jac1 = body1.sqrt_ii * ang_jac1;
        ang_jac2 = body2.sqrt_ii * ang_jac2;

        JointVelocityConstraint {
            joint_id,
            mj_lambda1: body1.mj_lambda,
            mj_lambda2: body2.mj_lambda,
            im1: body1.im,
            im2: body2.im,
            impulse: N::zero(),
            impulse_bounds: [-N::splat(Real::MAX), N::splat(Real::MAX)],
            lin_jac,
            ang_jac1,
            ang_jac2,
            inv_lhs: N::zero(), // Will be set during ortogonalization.
            cfm_coeff,
            cfm_gain: N::zero(),
            rhs: rhs_wo_bias + rhs_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    pub fn limit_angular<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        limited_axis: usize,
        limits: [N; 2],
        writeback_id: WritebackId,
    ) -> JointVelocityConstraint<N, LANES> {
        let zero = N::zero();
        let half = N::splat(0.5);
        let limits = [limits[0] * half, limits[1] * half];
        #[cfg(feature = "dim2")]
        let ang = self.ang_err;
        #[cfg(feature = "dim3")]
        let ang = self.ang_err[limited_axis];
        let min_enabled = ang.simd_lt(limits[0]);
        let max_enabled = limits[1].simd_lt(ang);

        let impulse_bounds = [
            N::splat(-Real::INFINITY).select(min_enabled, zero),
            N::splat(Real::INFINITY).select(max_enabled, zero),
        ];

        #[cfg(feature = "dim2")]
        let ang_jac = self.basis[limited_axis];
        #[cfg(feature = "dim3")]
        let ang_jac = self.basis.column(limited_axis).into_owned();
        let dvel = ang_jac.gdot(body2.angvel) - ang_jac.gdot(body1.angvel);
        let rhs_wo_bias = dvel;

        let erp_inv_dt = N::splat(params.joint_erp_inv_dt());
        let cfm_coeff = N::splat(params.joint_cfm_coeff());
        let rhs_bias =
            ((ang - limits[1]).simd_max(zero) - (limits[0] - ang).simd_max(zero)) * erp_inv_dt;

        let ang_jac1 = body1.sqrt_ii * ang_jac;
        let ang_jac2 = body2.sqrt_ii * ang_jac;

        JointVelocityConstraint {
            joint_id,
            mj_lambda1: body1.mj_lambda,
            mj_lambda2: body2.mj_lambda,
            im1: body1.im,
            im2: body2.im,
            impulse: N::zero(),
            impulse_bounds,
            lin_jac: na::zero(),
            ang_jac1,
            ang_jac2,
            inv_lhs: N::zero(), // Will be set during ortogonalization.
            cfm_coeff,
            cfm_gain: N::zero(),
            rhs: rhs_wo_bias + rhs_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    pub fn motor_angular<const LANES: usize>(
        &self,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        _motor_axis: usize,
        motor_params: &MotorParameters<N>,
        writeback_id: WritebackId,
    ) -> JointVelocityConstraint<N, LANES> {
        // let mut ang_jac = self.basis.column(_motor_axis).into_owned();
        #[cfg(feature = "dim2")]
        let ang_jac = N::one();
        #[cfg(feature = "dim3")]
        let ang_jac = self.basis.column(_motor_axis).into_owned();

        let mut rhs_wo_bias = N::zero();
        if motor_params.erp_inv_dt != N::zero() {
            #[cfg(feature = "dim2")]
            let ang_dist = self.ang_err;
            #[cfg(feature = "dim3")]
            let ang_dist = self.ang_err[_motor_axis];
            let target_ang = motor_params.target_pos;
            rhs_wo_bias += (ang_dist - target_ang) * motor_params.erp_inv_dt;
        }

        let dvel = ang_jac.gdot(body2.angvel) - ang_jac.gdot(body1.angvel);
        rhs_wo_bias += dvel - motor_params.target_vel;

        let ang_jac1 = body1.sqrt_ii * ang_jac;
        let ang_jac2 = body2.sqrt_ii * ang_jac;

        JointVelocityConstraint {
            joint_id,
            mj_lambda1: body1.mj_lambda,
            mj_lambda2: body2.mj_lambda,
            im1: body1.im,
            im2: body2.im,
            impulse: N::zero(),
            impulse_bounds: [-motor_params.max_impulse, motor_params.max_impulse],
            lin_jac: na::zero(),
            ang_jac1,
            ang_jac2,
            inv_lhs: N::zero(), // Will be set during ortogonalization.
            cfm_coeff: motor_params.cfm_coeff,
            cfm_gain: motor_params.cfm_gain,
            rhs: rhs_wo_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    pub fn lock_angular<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        locked_axis: usize,
        writeback_id: WritebackId,
    ) -> JointVelocityConstraint<N, LANES> {
        #[cfg(feature = "dim2")]
        let ang_jac = self.basis[locked_axis];
        #[cfg(feature = "dim3")]
        let ang_jac = self.basis.column(locked_axis).into_owned();

        let dvel = ang_jac.gdot(body2.angvel) - ang_jac.gdot(body1.angvel);
        let rhs_wo_bias = dvel;

        let erp_inv_dt = N::splat(params.joint_erp_inv_dt());
        let cfm_coeff = N::splat(params.joint_cfm_coeff());
        #[cfg(feature = "dim2")]
        let rhs_bias = self.ang_err * erp_inv_dt;
        #[cfg(feature = "dim3")]
        let rhs_bias = self.ang_err[locked_axis] * erp_inv_dt;

        let ang_jac1 = body1.sqrt_ii * ang_jac;
        let ang_jac2 = body2.sqrt_ii * ang_jac;

        JointVelocityConstraint {
            joint_id,
            mj_lambda1: body1.mj_lambda,
            mj_lambda2: body2.mj_lambda,
            im1: body1.im,
            im2: body2.im,
            impulse: N::zero(),
            impulse_bounds: [-N::splat(Real::MAX), N::splat(Real::MAX)],
            lin_jac: na::zero(),
            ang_jac1,
            ang_jac2,
            inv_lhs: N::zero(), // Will be set during ortogonalization.
            cfm_coeff,
            cfm_gain: N::zero(),
            rhs: rhs_wo_bias + rhs_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    /// Orthogonalize the constraints and set their inv_lhs field.
    pub fn finalize_constraints<const LANES: usize>(
        constraints: &mut [JointVelocityConstraint<N, LANES>],
    ) {
        let len = constraints.len();

        if len == 0 {
            return;
        }

        let imsum = constraints[0].im1 + constraints[0].im2;

        // Use the modified Gram-Schmidt orthogonalization.
        for j in 0..len {
            let c_j = &mut constraints[j];
            let dot_jj = c_j.lin_jac.dot(&imsum.component_mul(&c_j.lin_jac))
                + c_j.ang_jac1.gdot(c_j.ang_jac1)
                + c_j.ang_jac2.gdot(c_j.ang_jac2);
            let cfm_gain = dot_jj * c_j.cfm_coeff + c_j.cfm_gain;
            let inv_dot_jj = crate::utils::simd_inv(dot_jj);
            c_j.inv_lhs = crate::utils::simd_inv(dot_jj + cfm_gain); // Don’t forget to update the inv_lhs.
            c_j.cfm_gain = cfm_gain;

            if c_j.impulse_bounds != [-N::splat(Real::MAX), N::splat(Real::MAX)] {
                // Don't remove constraints with limited forces from the others
                // because they may not deliver the necessary forces to fulfill
                // the removed parts of other constraints.
                continue;
            }

            for i in (j + 1)..len {
                let (c_i, c_j) = constraints.index_mut_const(i, j);

                let dot_ij = c_i.lin_jac.dot(&imsum.component_mul(&c_j.lin_jac))
                    + c_i.ang_jac1.gdot(c_j.ang_jac1)
                    + c_i.ang_jac2.gdot(c_j.ang_jac2);
                let coeff = dot_ij * inv_dot_jj;

                c_i.lin_jac -= c_j.lin_jac * coeff;
                c_i.ang_jac1 -= c_j.ang_jac1 * coeff;
                c_i.ang_jac2 -= c_j.ang_jac2 * coeff;
                c_i.rhs_wo_bias -= c_j.rhs_wo_bias * coeff;
                c_i.rhs -= c_j.rhs * coeff;
            }
        }
    }

    pub fn limit_linear_ground<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        limited_axis: usize,
        limits: [N; 2],
        writeback_id: WritebackId,
    ) -> JointVelocityGroundConstraint<N, LANES> {
        let zero = N::zero();
        let lin_jac = self.basis.column(limited_axis).into_owned();
        let dist = self.lin_err.dot(&lin_jac);

        let min_enabled = dist.simd_lt(limits[0]);
        let max_enabled = limits[1].simd_lt(dist);

        let impulse_bounds = [
            N::splat(-Real::INFINITY).select(min_enabled, zero),
            N::splat(Real::INFINITY).select(max_enabled, zero),
        ];

        let ang_jac1 = self.cmat1_basis.column(limited_axis).into_owned();
        #[cfg(feature = "dim2")]
        let mut ang_jac2 = self.cmat2_basis[limited_axis];
        #[cfg(feature = "dim3")]
        let mut ang_jac2 = self.cmat2_basis.column(limited_axis).into_owned();

        let dvel = lin_jac.dot(&(body2.linvel - body1.linvel))
            + (ang_jac2.gdot(body2.angvel) - ang_jac1.gdot(body1.angvel));
        let rhs_wo_bias = dvel;

        let erp_inv_dt = N::splat(params.joint_erp_inv_dt());
        let cfm_coeff = N::splat(params.joint_cfm_coeff());
        let rhs_bias =
            ((dist - limits[1]).simd_max(zero) - (limits[0] - dist).simd_max(zero)) * erp_inv_dt;

        ang_jac2 = body2.sqrt_ii * ang_jac2;

        JointVelocityGroundConstraint {
            joint_id,
            mj_lambda2: body2.mj_lambda,
            im2: body2.im,
            impulse: zero,
            impulse_bounds,
            lin_jac,
            ang_jac2,
            inv_lhs: zero, // Will be set during ortogonalization.
            cfm_coeff,
            cfm_gain: N::zero(),
            rhs: rhs_wo_bias + rhs_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    pub fn motor_linear_ground<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        motor_axis: usize,
        motor_params: &MotorParameters<N>,
        limits: Option<[N; 2]>,
        writeback_id: WritebackId,
    ) -> JointVelocityGroundConstraint<N, LANES> {
        let inv_dt = N::splat(params.inv_dt());

        let lin_jac = self.basis.column(motor_axis).into_owned();
        let ang_jac1 = self.cmat1_basis.column(motor_axis).into_owned();
        #[cfg(feature = "dim2")]
        let mut ang_jac2 = self.cmat2_basis[motor_axis];
        #[cfg(feature = "dim3")]
        let mut ang_jac2 = self.cmat2_basis.column(motor_axis).into_owned();

        let mut rhs_wo_bias = N::zero();
        if motor_params.erp_inv_dt != N::zero() {
            let dist = self.lin_err.dot(&lin_jac);
            rhs_wo_bias += (dist - motor_params.target_pos) * motor_params.erp_inv_dt;
        }

        let mut target_vel = motor_params.target_vel;
        if let Some(limits) = limits {
            let dist = self.lin_err.dot(&lin_jac);
            target_vel =
                target_vel.simd_clamp((limits[0] - dist) * inv_dt, (limits[1] - dist) * inv_dt);
        };

        let dvel = lin_jac.dot(&(body2.linvel - body1.linvel))
            + (ang_jac2.gdot(body2.angvel) - ang_jac1.gdot(body1.angvel));
        rhs_wo_bias += dvel - target_vel;

        ang_jac2 = body2.sqrt_ii * ang_jac2;

        JointVelocityGroundConstraint {
            joint_id,
            mj_lambda2: body2.mj_lambda,
            im2: body2.im,
            impulse: N::zero(),
            impulse_bounds: [-motor_params.max_impulse, motor_params.max_impulse],
            lin_jac,
            ang_jac2,
            inv_lhs: N::zero(), // Will be set during ortogonalization.
            cfm_coeff: motor_params.cfm_coeff,
            cfm_gain: motor_params.cfm_gain,
            rhs: rhs_wo_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    pub fn lock_linear_ground<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        locked_axis: usize,
        writeback_id: WritebackId,
    ) -> JointVelocityGroundConstraint<N, LANES> {
        let lin_jac = self.basis.column(locked_axis).into_owned();
        let ang_jac1 = self.cmat1_basis.column(locked_axis).into_owned();
        #[cfg(feature = "dim2")]
        let mut ang_jac2 = self.cmat2_basis[locked_axis];
        #[cfg(feature = "dim3")]
        let mut ang_jac2 = self.cmat2_basis.column(locked_axis).into_owned();

        let dvel = lin_jac.dot(&(body2.linvel - body1.linvel))
            + (ang_jac2.gdot(body2.angvel) - ang_jac1.gdot(body1.angvel));
        let rhs_wo_bias = dvel;

        let erp_inv_dt = N::splat(params.joint_erp_inv_dt());
        let cfm_coeff = N::splat(params.joint_cfm_coeff());
        let rhs_bias = lin_jac.dot(&self.lin_err) * erp_inv_dt;

        ang_jac2 = body2.sqrt_ii * ang_jac2;

        JointVelocityGroundConstraint {
            joint_id,
            mj_lambda2: body2.mj_lambda,
            im2: body2.im,
            impulse: N::zero(),
            impulse_bounds: [-N::splat(Real::MAX), N::splat(Real::MAX)],
            lin_jac,
            ang_jac2,
            inv_lhs: N::zero(), // Will be set during ortogonalization.
            cfm_coeff,
            cfm_gain: N::zero(),
            rhs: rhs_wo_bias + rhs_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    pub fn motor_angular_ground<const LANES: usize>(
        &self,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        _motor_axis: usize,
        motor_params: &MotorParameters<N>,
        writeback_id: WritebackId,
    ) -> JointVelocityGroundConstraint<N, LANES> {
        // let mut ang_jac = self.basis.column(_motor_axis).into_owned();
        #[cfg(feature = "dim2")]
        let ang_jac = N::one();
        #[cfg(feature = "dim3")]
        let ang_jac = self.basis.column(_motor_axis).into_owned();

        let mut rhs_wo_bias = N::zero();
        if motor_params.erp_inv_dt != N::zero() {
            #[cfg(feature = "dim2")]
            let ang_dist = self.ang_err;
            #[cfg(feature = "dim3")]
            let ang_dist = self.ang_err[_motor_axis];
            let target_ang = motor_params.target_pos;
            rhs_wo_bias += (ang_dist - target_ang) * motor_params.erp_inv_dt;
        }

        let dvel = ang_jac.gdot(body2.angvel) - ang_jac.gdot(body1.angvel);
        rhs_wo_bias += dvel - motor_params.target_vel;

        let ang_jac2 = body2.sqrt_ii * ang_jac;

        JointVelocityGroundConstraint {
            joint_id,
            mj_lambda2: body2.mj_lambda,
            im2: body2.im,
            impulse: N::zero(),
            impulse_bounds: [-motor_params.max_impulse, motor_params.max_impulse],
            lin_jac: na::zero(),
            ang_jac2,
            inv_lhs: N::zero(), // Will be set during ortogonalization.
            cfm_coeff: motor_params.cfm_coeff,
            cfm_gain: motor_params.cfm_gain,
            rhs: rhs_wo_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    pub fn limit_angular_ground<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        limited_axis: usize,
        limits: [N; 2],
        writeback_id: WritebackId,
    ) -> JointVelocityGroundConstraint<N, LANES> {
        let zero = N::zero();
        let half = N::splat(0.5);
        let limits = [limits[0] * half, limits[1] * half];
        #[cfg(feature = "dim2")]
        let ang = self.ang_err;
        #[cfg(feature = "dim3")]
        let ang = self.ang_err[limited_axis];
        let min_enabled = ang.simd_lt(limits[0]);
        let max_enabled = limits[1].simd_lt(ang);

        let impulse_bounds = [
            N::splat(-Real::INFINITY).select(min_enabled, zero),
            N::splat(Real::INFINITY).select(max_enabled, zero),
        ];

        #[cfg(feature = "dim2")]
        let ang_jac = self.basis[limited_axis];
        #[cfg(feature = "dim3")]
        let ang_jac = self.basis.column(limited_axis).into_owned();
        let dvel = ang_jac.gdot(body2.angvel) - ang_jac.gdot(body1.angvel);
        let rhs_wo_bias = dvel;

        let erp_inv_dt = N::splat(params.joint_erp_inv_dt());
        let cfm_coeff = N::splat(params.joint_cfm_coeff());
        let rhs_bias =
            ((ang - limits[1]).simd_max(zero) - (limits[0] - ang).simd_max(zero)) * erp_inv_dt;

        let ang_jac2 = body2.sqrt_ii * ang_jac;

        JointVelocityGroundConstraint {
            joint_id,
            mj_lambda2: body2.mj_lambda,
            im2: body2.im,
            impulse: zero,
            impulse_bounds,
            lin_jac: na::zero(),
            ang_jac2,
            inv_lhs: zero, // Will be set during ortogonalization.
            cfm_coeff,
            cfm_gain: N::zero(),
            rhs: rhs_wo_bias + rhs_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    pub fn lock_angular_ground<const LANES: usize>(
        &self,
        params: &IntegrationParameters,
        joint_id: [JointIndex; LANES],
        body1: &SolverBody<N, LANES>,
        body2: &SolverBody<N, LANES>,
        locked_axis: usize,
        writeback_id: WritebackId,
    ) -> JointVelocityGroundConstraint<N, LANES> {
        #[cfg(feature = "dim2")]
        let ang_jac = self.basis[locked_axis];
        #[cfg(feature = "dim3")]
        let ang_jac = self.basis.column(locked_axis).into_owned();
        let dvel = ang_jac.gdot(body2.angvel) - ang_jac.gdot(body1.angvel);
        let rhs_wo_bias = dvel;

        let erp_inv_dt = N::splat(params.joint_erp_inv_dt());
        let cfm_coeff = N::splat(params.joint_cfm_coeff());
        #[cfg(feature = "dim2")]
        let rhs_bias = self.ang_err * erp_inv_dt;
        #[cfg(feature = "dim3")]
        let rhs_bias = self.ang_err[locked_axis] * erp_inv_dt;

        let ang_jac2 = body2.sqrt_ii * ang_jac;

        JointVelocityGroundConstraint {
            joint_id,
            mj_lambda2: body2.mj_lambda,
            im2: body2.im,
            impulse: N::zero(),
            impulse_bounds: [-N::splat(Real::MAX), N::splat(Real::MAX)],
            lin_jac: na::zero(),
            ang_jac2,
            inv_lhs: N::zero(), // Will be set during ortogonalization.
            cfm_coeff,
            cfm_gain: N::zero(),
            rhs: rhs_wo_bias + rhs_bias,
            rhs_wo_bias,
            writeback_id,
        }
    }

    /// Orthogonalize the constraints and set their inv_lhs field.
    pub fn finalize_ground_constraints<const LANES: usize>(
        constraints: &mut [JointVelocityGroundConstraint<N, LANES>],
    ) {
        let len = constraints.len();

        if len == 0 {
            return;
        }

        let imsum = constraints[0].im2;

        // Use the modified Gram-Schmidt orthogonalization.
        for j in 0..len {
            let c_j = &mut constraints[j];
            let dot_jj = c_j.lin_jac.dot(&imsum.component_mul(&c_j.lin_jac))
                + c_j.ang_jac2.gdot(c_j.ang_jac2);
            let cfm_gain = dot_jj * c_j.cfm_coeff + c_j.cfm_gain;
            let inv_dot_jj = crate::utils::simd_inv(dot_jj + cfm_gain);
            c_j.inv_lhs = inv_dot_jj; // Don’t forget to update the inv_lhs.
            c_j.cfm_gain = cfm_gain;

            if c_j.impulse_bounds != [-N::splat(Real::MAX), N::splat(Real::MAX)]
                || c_j.cfm_gain != N::zero()
            {
                // Don't remove constraints with limited forces from the others
                // because they may not deliver the necessary forces to fulfill
                // the removed parts of other constraints.
                continue;
            }

            for i in j + 1..len {
                let (c_i, c_j) = constraints.index_mut_const(i, j);

                let dot_ij = c_i.lin_jac.dot(&imsum.component_mul(&c_j.lin_jac))
                    + c_i.ang_jac2.gdot(c_j.ang_jac2);
                let coeff = dot_ij * inv_dot_jj;

                c_i.lin_jac -= c_j.lin_jac * coeff;
                c_i.ang_jac2 -= c_j.ang_jac2 * coeff;
                c_i.rhs_wo_bias -= c_j.rhs_wo_bias * coeff;
                c_i.rhs -= c_j.rhs * coeff;
            }
        }
    }
}
