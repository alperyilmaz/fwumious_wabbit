use std::mem::{self, MaybeUninit};
//use fastapprox::fast::sigmoid;
use std::process;
use triomphe::{UniqueArc, Arc};
//use std::fmt::Binary;
use core::arch::x86_64::*;
use merand48::*;

use crate::model_instance;
use crate::feature_buffer;
use crate::feature_buffer::HashAndValue;


const ONE:u32 = 1065353216;// this is 1.0 float -> u32
const BUF_LEN:usize = 16196; // We will ABORT if number of derived features for individual example is more than this.
// Why not bigger number? If we grow stack of the function too much, we end up with stack overflow protecting mechanisms

// this really means 7 bits of mantissa and 4 bits of fixed point precision
const FASTMATH_LR_LUT_BITS:u8 = 11;
const FASTMATH_LR_LUT_SIZE:usize = 1 <<  FASTMATH_LR_LUT_BITS;

#[derive(Clone)]
pub struct Weight {
    weight: f32, 
    acc_grad: f32,
}

pub struct Regressor {
    hash_mask: u32,
    learning_rate: f32,
    minus_power_t:f32,
    pub weights: Vec<Weight>,       // weights and gradients, interleved, for all models
    ffm_weights_offset: u32, 
    ffm_k: u32,
    ffm_hashmask: u32,
    ffm_one_over_k_root: f32,
    ffm_separate_vectors_k: u32,
    fastmath: bool,
    fastmath_lr_lut: [f32; FASTMATH_LR_LUT_SIZE],
    ffm_iw_weights_offset: u32,
}

#[derive(Clone)]
pub struct FixedRegressor {
    hash_mask: u32,
    pub weights: Arc<Vec<Weight>>,       // Both weights and gradients
    ffm_weights_offset: u32, 
    ffm_k: u32,
    ffm_hashmask: u32,
    ffm_separate_vectors_k: u32,
}


macro_rules! specialize_boolean {
    ( $input_expr:expr, 
      $output_const:ident,
      $code_block:block  ) => {
         match $input_expr {
                true => {const $output_const:bool = true; $code_block},
                false => {const $output_const:bool = false; $code_block},
            }
    };
}


static mut SUM_GRADIENTS: f64 = 0.0;
static mut NUM_GRADIENTS: u64 = 0;

impl Regressor {
    
    pub fn new(model_instance: &model_instance::ModelInstance) -> Regressor {
        let hash_mask = (1 << model_instance.bit_precision) -1;
        let lr_weights_len = hash_mask + 1;
        let mut rg = Regressor{
                            hash_mask: hash_mask,
                            learning_rate: model_instance.learning_rate,
                            minus_power_t : - model_instance.power_t,
                            //minimum_learning_rate: model_instance.minimum_learning_rate,
                            weights: Vec::new(), 
                            ffm_weights_offset: 0,
                            ffm_k: 0,
                            ffm_hashmask: 0,
                            ffm_one_over_k_root: 0.0,
                            ffm_separate_vectors_k: 0,
                            fastmath: model_instance.fastmath,
                            fastmath_lr_lut: [0.0; FASTMATH_LR_LUT_SIZE],
                            ffm_iw_weights_offset: 0,
                        };

        if rg.fastmath {
            rg.fastmath_create_lut(rg.learning_rate, rg.minus_power_t);
        }

        let mut ffm_weights_len = 0;
        if model_instance.ffm_k > 0 {
            // To keep things simple ffm weights length will be the same as lr
            ffm_weights_len = 1 << model_instance.ffm_bit_precision;
            rg.ffm_weights_offset = lr_weights_len;
            rg.ffm_k = model_instance.ffm_k;
            // Since we will align our dimensions, we need to know the number of bits for them
            let mut ffm_bits_for_dimensions = 0;
            while rg.ffm_k > (1 << (ffm_bits_for_dimensions)) {
                ffm_bits_for_dimensions += 1;
            }
            let dimensions_mask = (1 << ffm_bits_for_dimensions) - 1;
            // in ffm we will simply mask the lower bits, so we spare them for k
            rg.ffm_hashmask = ((1 << model_instance.ffm_bit_precision) -1) ^ dimensions_mask;
        }
        // Now allocate weights
        let iw_weights_len =rg.ffm_k * (model_instance.ffm_fields.len() as u32 * model_instance.ffm_fields.len() as u32);
        rg.ffm_iw_weights_offset = lr_weights_len + ffm_weights_len;
        rg.weights = vec![Weight{weight:0.0, acc_grad:0.0}; (lr_weights_len + ffm_weights_len + iw_weights_len) as usize];

        // Initialization, from ffm.pdf, however should random distribution be centred on zero?
        if model_instance.ffm_k > 0 {       
            rg.ffm_one_over_k_root = 1.0 / (rg.ffm_k as f32).sqrt() / 10.0;
            for i in 0..ffm_weights_len {
//                let mk = (i % rg.ffm_k) + 1;
//                println!("i: {}, mk: {}, rg_ffm: {}", i, mk, rg.ffm_k);
                rg.weights[(rg.ffm_weights_offset + i) as usize].weight = 0.0;//(0.2*merand48((rg.ffm_weights_offset+i) as u64)-0.1) * rg.ffm_one_over_k_root;
                //rng.gen_range(-0.1 * rg.ffm_one_over_k_root , 0.1 * rg.ffm_one_over_k_root );
                // we set FFM gradients to 1.0, so we avoid NaN updates due to adagrad (accumulated_squared_gradients+grad^2).powf(negative_number) * 0.0 
                rg.weights[(rg.ffm_weights_offset + i) as usize].acc_grad = 1.0;
            }
/*            for i in 0..(rg.ffm_k * model_instance.ffm_fields.len()  as u32 * model_instance.ffm_fields.len() as u32) {
                rg.weights[(rg.ffm_iw_weights_offset + i) as usize].weight = 1.0;
                rg.weights[(rg.ffm_iw_weights_offset + i) as usize].acc_grad = 1.0;
            }*/
            if model_instance.ffm_separate_vectors {
                rg.ffm_separate_vectors_k = rg.ffm_k;
            }
        }
        rg
    }
    
    pub fn fastmath_create_lut(&mut self, learning_rate: f32, minus_power_t: f32) {
        println!("Using look-up tables for AdaGrad learning rate calculation");
        for x in 0..FASTMATH_LR_LUT_SIZE {
            // accumulated gradients are always positive floating points, sign is guaranteed to be zero
            // floating point: 1 bit of sign, 7 bits of signed mantissa, then floating point bits
            // we will take 7 bits of mantissa + whatever most significant bits remain
            // we take two consequential such values, so in effect lookup acts as rounding
            let float_x = f32::from_bits((x as u32)  << (31-FASTMATH_LR_LUT_BITS));
            let float_x_plus_one = f32::from_bits(((x+1) as u32)  << (31-FASTMATH_LR_LUT_BITS));
            let mut val = learning_rate * ((float_x).powf(minus_power_t) + (float_x_plus_one).powf(minus_power_t))/2.0;
            /*if val > learning_rate || val.is_nan(){
                val = learning_rate;
            }*/
            self.fastmath_lr_lut[x] = val;
        }
    }
    
    #[inline(always)]
    unsafe fn fastmath_calculate_update(&self, update: f32, accumulated_squared_gradient: f32) -> f32 {
            debug_assert!(accumulated_squared_gradient >= 0.0);
            let key = accumulated_squared_gradient.to_bits() >> (31-FASTMATH_LR_LUT_BITS);
            return update * *self.fastmath_lr_lut.get_unchecked(key as usize);
    }

    pub fn learn(&mut self, fb: &feature_buffer::FeatureBuffer, update: bool, example_num: u32) -> f32 {
        pub struct IndexAccgradientValue {
            index: u32,
            accgradient: f32,
            value: f32,
        }

        unsafe {
        let y = fb.label; // 0.0 or 1.0
        let fbuf = &fb.lr_buffer;
        let fbuf_len = fbuf.len();
        let mut local_buf_len = fbuf_len;
        if local_buf_len > BUF_LEN {
            println!("Number of features per example ({}) is higher than supported in this fw binary ({}), exiting", local_buf_len, BUF_LEN);
            process::exit(1);
        }
        let mut local_data: [IndexAccgradientValue; BUF_LEN as usize] = MaybeUninit::uninit().assume_init() ;
        let mut wsum:f32 = 0.0;
        for i in 0..fbuf_len {
// For now this didn't bear fruit
//            _mm_prefetch(mem::transmute::<&f32, &i8>(self.weights.get_unchecked((fbuf.get_unchecked(i+8).hash << 1) as usize)), _MM_HINT_T0); 
            let feature_index     = fbuf.get_unchecked(i).hash;
            let feature_value:f32 = fbuf.get_unchecked(i).value;
            let feature_weight       = self.weights.get_unchecked(feature_index as usize).weight;
            let accumulated_gradient = self.weights.get_unchecked(feature_index as usize).acc_grad;
            wsum += feature_weight * feature_value;
            local_data.get_unchecked_mut(i).index       = feature_index;
            local_data.get_unchecked_mut(i).accgradient = accumulated_gradient;
            local_data.get_unchecked_mut(i).value       = feature_value;
        }
        
        if self.ffm_k > 0 {
            for (i, left_fbuf) in fb.ffm_buffers.iter().enumerate() {
                for left_hash in left_fbuf {
                    let left_feature_value = left_hash.value;
                    for (j, right_fbuf) in fb.ffm_buffers[i+1 ..].iter().enumerate() {
                        let mut left_weight_p = (self.ffm_weights_offset + ((left_hash.hash + (i+1+j) as u32 *self.ffm_separate_vectors_k) & self.ffm_hashmask)) as usize;
                        for right_hash in right_fbuf {
                            let right_feature_value = right_hash.value;
                            let joint_value = left_feature_value * right_feature_value;
                            let mut right_weight_p = (self.ffm_weights_offset + ((right_hash.hash + i as u32 * self.ffm_separate_vectors_k) & self.ffm_hashmask)) as usize;
                            //let mut iw_weight_p = (self.ffm_iw_weights_offset as usize + self.ffm_k as usize * 2*(i * fb.ffm_buffers.len() + (i+1+j))) as usize;
                                
                            for _ in (0..(self.ffm_k as usize)).rev() {
  //                              let iw_weight = self.weights.get_unchecked(iw_weight_p as usize);
                                let left_weight = self.weights.get_unchecked(left_weight_p).weight;
                                let right_weight = self.weights.get_unchecked(right_weight_p).weight;
  
                                let right_half_part = right_weight * joint_value;
                                // left side
                                local_data.get_unchecked_mut(local_buf_len).index = left_weight_p as u32;// store index
                                local_data.get_unchecked_mut(local_buf_len).accgradient = self.weights.get_unchecked(left_weight_p).acc_grad; // accumulated errors
                                local_data.get_unchecked_mut(local_buf_len).value = right_half_part; // first derivate
                                // right side
                                local_data.get_unchecked_mut(local_buf_len+1).index = right_weight_p as u32; // store index
                                local_data.get_unchecked_mut(local_buf_len+1).accgradient = self.weights.get_unchecked(right_weight_p).acc_grad; // accumulated errors
                                local_data.get_unchecked_mut(local_buf_len+1).value = joint_value *  left_weight; // first derivate
                                
                                wsum += left_weight * right_half_part;
                                local_buf_len += 2;
                                left_weight_p += 1;
                                right_weight_p += 1;
                                /*local_data.get_unchecked_mut(local_buf_len+2).index = iw_weight_p as u32; // store index
                                local_data.get_unchecked_mut(local_buf_len+2).accgradient = *self.weights.get_unchecked(iw_weight_p +1); // accumulated errors
                                local_data.get_unchecked_mut(local_buf_len+2).value = left_weight * right_weight * left_feature_value * right_feature_value; // first derivate

                                local_buf_len += 1;

                                iw_weight_p += 1;*/
                            }
                            /*
                            println!("A {} {} {} {} {} {}", i,j+i+1, left_hash, right_hash, 
                                                            self.weights[left_weight_p], 
                                                            self.weights[right_weight_p]);
                            */
                        }
                    }
                }
            }
            
        }
        // Trick: instead of multiply in the updates with learning rate, multiply the result
        let prediction = -wsum;
        // vowpal compatibility
        if prediction.is_nan() {
            eprintln!("NAN prediction in example {}, forcing 0.0", example_num);
            let prediction_finalized:f32 = 0.0;
            return (1.0+(prediction_finalized).exp()).recip();
        } else if prediction < -50.0 {
            let prediction_finalized:f32 = -50.0;
            return (1.0+(prediction_finalized).exp()).recip();
        } else if prediction > 50.0 {
            let prediction_finalized:f32 = 50.0;
            return (1.0+(prediction_finalized).exp()).recip();
        }

        let prediction_probability:f32 = (1.0+(prediction).exp()).recip();

        if update {
            let general_gradient = y - prediction_probability;
            specialize_boolean!(self.fastmath, FASTMATH, {
                for i in 0..local_buf_len {
                    let feature_index = local_data.get_unchecked(i).index as usize;
                    let feature_value = local_data.get_unchecked(i).value;
                    let gradient = general_gradient * feature_value;
                    let gradient_squared = gradient*gradient;
                    self.weights.get_unchecked_mut(feature_index).acc_grad += gradient_squared;
//                    _mm_prefetch(mem::transmute::<&f32, &i8>(self.weights.get_unchecked_mut(local_data.get_unchecked(i+1).index as usize)), _MM_HINT_T1); 
                    let accumulated_squared_gradient = local_data.get_unchecked(i).accgradient;
                    if FASTMATH {
                        let update = self.fastmath_calculate_update(gradient, accumulated_squared_gradient + gradient_squared);
                        self.weights.get_unchecked_mut(feature_index).weight += update;
                    } else {
                        let learning_rate = self.learning_rate * (accumulated_squared_gradient + gradient_squared).powf(self.minus_power_t);
                        let update = gradient * learning_rate;
                        self.weights.get_unchecked_mut(feature_index).weight += update;
                    }
  //                  SUM_GRADIENTS += gradient_squared as f64;
  //                  NUM_GRADIENTS += 1;
                }            
            });
        }
  //      if example_num % 100000 == 0{
  //          println!("{}: {}", NUM_GRADIENTS, SUM_GRADIENTS/(NUM_GRADIENTS as f64));
  //      }
        
        prediction_probability
        }
    }
}

impl FixedRegressor {
    pub fn new(rr: Regressor) -> FixedRegressor {
        FixedRegressor {
                        hash_mask: rr.hash_mask,
                        weights: Arc::new(rr.weights),
                        ffm_weights_offset: rr.ffm_weights_offset,
                        ffm_k: rr.ffm_k,
                        ffm_hashmask: rr.ffm_hashmask,
                        ffm_separate_vectors_k: rr.ffm_separate_vectors_k,

        }
    }

    pub fn predict(&self, fb: &feature_buffer::FeatureBuffer, example_num: u32) -> f32 {
        let fbuf = &fb.lr_buffer;
        let mut wsum:f32 = 0.0;
        for val in fbuf {
            let hash = val.hash as usize;
            let feature_value:f32 = val.value;
            let w = self.weights[hash].weight;
            wsum += w * feature_value;    
        }
        

        if self.ffm_k > 0 {
            let left_feature_value = 1.0;  // we currently do not support feature values in ffm
            let right_feature_value = 1.0;
            for (i, left_fbuf) in fb.ffm_buffers.iter().enumerate() {
                for left_hash in left_fbuf {
                    let left_weight_p = (self.ffm_weights_offset + (left_hash.hash & self.ffm_hashmask)) as usize;
                    for (j, right_fbuf) in fb.ffm_buffers[i+1 ..].iter().enumerate() {
                        for right_hash in right_fbuf {
                            let right_weight_p = (self.ffm_weights_offset + (right_hash.hash & self.ffm_hashmask)) as usize;
                            for k in 0..(self.ffm_k as usize) {
                                let left_weight = self.weights[left_weight_p + k].weight;
                                let right_weight = self.weights[right_weight_p + k].weight;
                                wsum += left_weight * right_weight * 
                                        left_feature_value * right_feature_value;
                            }
                        }
                    }
                }
            }
            
        }


        let prediction = -wsum;
        let mut prediction_finalized = prediction;
        if prediction_finalized.is_nan() {
            eprintln!("NAN prediction in example {}, forcing 0.0", example_num);
            prediction_finalized = 0.0;
        } else if prediction_finalized < -50.0 {
            prediction_finalized = -50.0;
        } else if prediction_finalized > 50.0 {
            prediction_finalized = 50.0;
        }
        let prediction_probability:f32 = (1.0+(prediction_finalized).exp()).recip();
        prediction_probability
    }

} 




mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    /* LR TESTS */

    fn lr_vec(v:Vec<feature_buffer::HashAndValue>) -> feature_buffer::FeatureBuffer {
        feature_buffer::FeatureBuffer {
                    label: 0.0,
                    lr_buffer: v,
                    ffm_buffers: Vec::new(),
        }
    }


    #[test]
    fn test_learning_turned_off() {
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.power_t = 0.0;
        mi.bit_precision = 18;
        
        let mut rr = Regressor::new(&mi);
        let mut p: f32;
        
        // Empty model: no matter how many features, prediction is 0.5
        p = rr.learn(&lr_vec(vec![]), false, 0);
        assert_eq!(p, 0.5);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}]), false, 0);
        assert_eq!(p, 0.5);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}, HashAndValue{hash:2, value: 1.0}]), false, 0);
        assert_eq!(p, 0.5);
    }

    #[test]
    fn test_power_t_zero() {
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.power_t = 0.0;
        mi.bit_precision = 18;
        
        let mut rr = Regressor::new(&mi);
        let mut p: f32;
        
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}]), true, 0);
        assert_eq!(p, 0.5);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}]), true, 0);
        assert_eq!(p, 0.48750263);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}]), true, 0);
        assert_eq!(p, 0.47533244);
    }

    #[test]
    fn test_double_same_feature() {
        // this is a tricky test - what happens on collision
        // depending on the order of math, results are different
        // so this is here, to make sure the math is always the same
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.power_t = 0.0;
        mi.bit_precision = 18;
        
        let mut rr = Regressor::new(&mi);
        let mut p: f32;
        let two = 2.0_f32.to_bits();
        
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}, HashAndValue{hash: 1, value: 2.0}]), true, 0);
        assert_eq!(p, 0.5);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}, HashAndValue{hash: 1, value: 2.0}]), true, 0);
        assert_eq!(p, 0.38936076);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}, HashAndValue{hash: 1, value: 2.0}]), true, 0);
        assert_eq!(p, 0.30993468);
    }


    #[test]
    fn test_power_t_half() {
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.power_t = 0.5;
        mi.bit_precision = 18;
        
        let mut rr = Regressor::new(&mi);
        let mut p: f32;
        
        p = rr.learn(&lr_vec(vec![HashAndValue{hash:1, value: 1.0}]), true, 0);
        assert_eq!(p, 0.5);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash:1, value: 1.0}]), true, 0);
        assert_eq!(p, 0.4750208);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash:1, value: 1.0}]), true, 0);
        assert_eq!(p, 0.45788094);
    }

    #[test]
    fn test_power_t_half_fastmath() {
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.power_t = 0.5;
        mi.fastmath = true;
        mi.bit_precision = 18;
        
        let mut rr = Regressor::new(&mi);
        let mut p: f32;
        
        p = rr.learn(&lr_vec(vec![HashAndValue{hash:1, value: 1.0}]), true, 0);
        assert_eq!(p, 0.5);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash:1, value: 1.0}]), true, 0);
        if FASTMATH_LR_LUT_BITS == 12 { 
            assert_eq!(p, 0.47539312); // when LUT = 12
        } else {
            assert_eq!(p, 0.475734);
        }
    }

    #[test]
    fn test_power_t_half_two_features() {
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.power_t = 0.5;
        mi.bit_precision = 18;
        
        let mut rr = Regressor::new(&mi);
        let mut p: f32;
        // Here we take twice two features and then once just one
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}, HashAndValue{hash:2, value: 1.0}]), true, 0);
        assert_eq!(p, 0.5);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}, HashAndValue{hash:2, value: 1.0}]), true, 0);
        assert_eq!(p, 0.45016602);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash: 1, value: 1.0}]), true, 0);
        assert_eq!(p, 0.45836908);
    }

    #[test]
    fn test_non_one_weight() {
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.power_t = 0.0;
        mi.bit_precision = 18;
        
        let mut rr = Regressor::new(&mi);
        let mut p: f32;
        
        p = rr.learn(&lr_vec(vec![HashAndValue{hash:1, value: 2.0}]), true, 0);
        assert_eq!(p, 0.5);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash:1, value: 2.0}]), true, 0);
        assert_eq!(p, 0.45016602);
        p = rr.learn(&lr_vec(vec![HashAndValue{hash:1, value: 2.0}]), true, 0);
        assert_eq!(p, 0.40611085);
    }

/* FFM TESTS */
    fn ffm_vec(v:Vec<Vec<feature_buffer::HashAndValue>>) -> feature_buffer::FeatureBuffer {
        feature_buffer::FeatureBuffer {
                    label: 0.0,
                    lr_buffer: Vec::new(),
                    ffm_buffers: v,
        }
    }

    fn ffm_fixed_init(mut rg: &mut Regressor) -> () {
        for i in rg.ffm_weights_offset as usize..rg.weights.len() {
            rg.weights[i].weight = 1.0;
            rg.weights[i].acc_grad = 1.0;
        }
    }


    #[test]
    fn test_ffm() {
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.power_t = 0.0;
        mi.bit_precision = 18;
        mi.ffm_k = 1;
        mi.ffm_bit_precision = 18;
        mi.ffm_fields = vec![vec![]]; // This isn't really used
        
        let mut p: f32;
        
        // Nothing can be learned from a single field
        let mut rr = Regressor::new(&mi);
        let ffm_buf = ffm_vec(vec![vec![HashAndValue{hash:1, value: 1.0}]]);
        p = rr.learn(&ffm_buf, true, 0);
        assert_eq!(p, 0.5);
        p = rr.learn(&ffm_buf, true, 0);
        assert_eq!(p, 0.5);

        // With two fields, things start to happen
        // Since fields depend on initial randomization, it's hard to stabilize that.
        let mut rr = Regressor::new(&mi);
        ffm_fixed_init(&mut rr);
        let ffm_buf = ffm_vec(vec![
                                  vec![HashAndValue{hash:1, value: 1.0}],
                                  vec![HashAndValue{hash:100, value: 1.0}]
                                  ]);
        p = rr.learn(&ffm_buf, true, 0);
        assert_eq!(p, 0.7310586); 
        p = rr.learn(&ffm_buf, true, 0);
        assert_eq!(p, 0.7024794);

        // Two fields, use values
        let mut rr = Regressor::new(&mi);
        ffm_fixed_init(&mut rr);
        let ffm_buf = ffm_vec(vec![
                                  vec![HashAndValue{hash:1, value: 2.0}],
                                  vec![HashAndValue{hash:100, value: 2.0}]
                                  ]);
        p = rr.learn(&ffm_buf, true, 0);
        assert_eq!(p, 0.98201376);
        p = rr.learn(&ffm_buf, true, 0);
        assert_eq!(p, 0.81377685);


    }


}







