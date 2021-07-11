/* This isn't really attention, this is more field weighting, we could call these "Field-weighted Field-aware Factorization Machines" */
use std::any::Any;
use std::io;
use merand48::*;
use core::arch::x86_64::*;
use std::error::Error;
use std::mem::{self, MaybeUninit};
use std::fs;
use std::path;


use crate::optimizer;
use crate::regressor;
use crate::model_instance;
use crate::feature_buffer;
use crate::consts;
use crate::block_helpers;
use optimizer::OptimizerTrait;
use regressor::BlockTrait;
use block_helpers::{Weight, WeightAndOptimizerData};

use crate::consts::AFFM_FOR;

const FFM_STACK_BUF_LEN:usize= 16384;
const FFM_CONTRA_BUF_LEN:usize = 8196;


const SQRT_OF_ONE_HALF:f32 = 0.70710678118;
 
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

const TRUNCATE:f32 = 0.2;


pub struct BlockAFFM<L:OptimizerTrait> {
    pub optimizer_ffm: L,
    pub optimizer_attention: L,
    pub local_ffm_derivatives: Vec<f32>,
    pub ffm_k: u32,
    pub ffm_weights_len: u32, 
    pub attention_weights_len: u32, 
    pub field_embedding_len: u32,
    pub attention_l2: f32,
    pub attention_snap_to_zero: f32,
    pub weights: Vec<WeightAndOptimizerData<L>>,
    pub attention_weights: Vec<WeightAndOptimizerData<L>>,
}


macro_rules! specialize_value_f32 {
    ( $input_expr:expr,
      $special_value: expr, 
      $output_const:ident,
      $code_block:block  ) => {
          if $input_expr == $special_value {	
              const $output_const:f32 = $special_value; 
              $code_block
          } else {
              let $output_const:f32 = $input_expr; 
              $code_block
          }
      };
}


macro_rules! specialize_k {
    ( $input_expr: expr, 
      $output_const: ident,
      $wsumbuf: ident,
      $code_block: block  ) => {
         match $input_expr {
                2 => {const $output_const:u32 = 2;   let mut $wsumbuf: [f32;$output_const as usize] = [0.0;$output_const as usize]; $code_block},
                4 => {const $output_const:u32 = 4;   let mut $wsumbuf: [f32;$output_const as usize] = [0.0;$output_const as usize]; $code_block},
                8 => {const $output_const:u32 = 8;   let mut $wsumbuf: [f32;$output_const as usize] = [0.0;$output_const as usize]; $code_block},
                val => {let $output_const:u32 = val; let mut $wsumbuf: [f32;consts::FFM_MAX_K] = [0.0;consts::FFM_MAX_K];      $code_block},
            }
    };
}



impl <L:OptimizerTrait + 'static> BlockTrait for BlockAFFM<L>

 {
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn new_without_weights(mi: &model_instance::ModelInstance) -> Result<Box<dyn BlockTrait>, Box<dyn Error>> {

        let mut reg_ffm = BlockAFFM::<L> {
            weights: Vec::new(),
            attention_weights: Vec::new(),
            attention_weights_len: 0,
            attention_l2: mi.attention_l2,
            attention_snap_to_zero: mi.attention_snap_to_zero,
            ffm_weights_len: 0, 
            local_ffm_derivatives: Vec::with_capacity(1024),
            ffm_k: mi.ffm_k, 
            field_embedding_len: mi.ffm_k * mi.ffm_fields.len() as u32,
            optimizer_ffm: L::new(),
            optimizer_attention: L::new(),
        };

        if mi.ffm_k > 0 {
            reg_ffm.optimizer_ffm.init(mi.ffm_learning_rate, mi.ffm_power_t, mi.ffm_init_acc_gradient);
            // Best params till now: 0.1, 0.2, 0.0 and random weights initialization
            //self.attention_weights[z].weight = (1.0 * merand48((self.ffm_weights_len as usize + z) as u64)-0.5) * 
//            reg_ffm.optimizer_attention.init(0.1, 0.25, 0.0); //BEST ON PROD
            reg_ffm.optimizer_attention.init(mi.attention_learning_rate, mi.attention_power_t, mi.attention_init_acc_gradient);
            // At the end we add "spillover buffer", so we can do modulo only on the base address and add offset
            reg_ffm.ffm_weights_len = (1 << mi.ffm_bit_precision) + (mi.ffm_fields.len() as u32 * reg_ffm.ffm_k);
            reg_ffm.attention_weights_len = (mi.ffm_fields.len() * mi.ffm_fields.len()) as u32;
        }

        // Verify that forward pass will have enough stack for temporary buffer
        if reg_ffm.ffm_k as usize * mi.ffm_fields.len() * mi.ffm_fields.len() > FFM_CONTRA_BUF_LEN {
            return Err(format!("FFM_CONTRA_BUF_LEN is {}. It needs to be at least ffm_k * number_of_fields^2. number_of_fields: {}, ffm_k: {}, please recompile with larger constant", 
                        FFM_CONTRA_BUF_LEN, mi.ffm_fields.len(), reg_ffm.ffm_k))?;
        }

        Ok(Box::new(reg_ffm))
    }

    fn new_forward_only_without_weights(&self) -> Result<Box<dyn BlockTrait>, Box<dyn Error>> {
        let forwards_only = BlockAFFM::<optimizer::OptimizerSGD> {
            weights: Vec::new(),
            attention_weights: Vec::new(),
            ffm_weights_len: self.ffm_weights_len, 
            attention_weights_len: self.attention_weights_len,
            attention_l2: self.attention_l2,
            attention_snap_to_zero: self.attention_snap_to_zero,
            local_ffm_derivatives: Vec::new(),
            ffm_k: self.ffm_k, 
            field_embedding_len: self.field_embedding_len,
            optimizer_ffm: optimizer::OptimizerSGD::new(),
            optimizer_attention: optimizer::OptimizerSGD::new(),
        };
        
        Ok(Box::new(forwards_only))
    }



    fn allocate_and_init_weights(&mut self, mi: &model_instance::ModelInstance) {
        self.weights =vec![WeightAndOptimizerData::<L>{weight:0.0, optimizer_data: self.optimizer_ffm.initial_data()}; self.ffm_weights_len as usize];
        self.attention_weights =vec![WeightAndOptimizerData::<L>{weight:0.0, optimizer_data: self.optimizer_attention.initial_data()}; self.attention_weights_len as usize];
        if mi.ffm_k > 0 {       
            if mi.ffm_init_width == 0.0 {
                // Initialization that has showed to work ok for us, like in ffm.pdf, but centered around zero and further divided by 50
                let ffm_one_over_k_root = 1.0 / (self.ffm_k as f32).sqrt() / 50.0;
                for i in 0..self.ffm_weights_len {
                    self.weights[i as usize].weight = (1.0 * merand48((self.ffm_weights_len as usize+ i as usize) as u64)-0.5) * ffm_one_over_k_root;
                    self.weights[i as usize].optimizer_data = self.optimizer_ffm.initial_data();
                }
            } else {
                let zero_half_band_width = mi.ffm_init_width * mi.ffm_init_zero_band * 0.5;
                let band_width = mi.ffm_init_width * (1.0 - mi.ffm_init_zero_band);
                for i in 0..self.ffm_weights_len {
                    let mut w = merand48(i as u64) * band_width - band_width * 0.5;
                    if w > 0.0 { 
                        w += zero_half_band_width ;
                    } else {
                        w -= zero_half_band_width;
                    }
                    w += mi.ffm_init_center;
                    self.weights[i as usize].weight = w; 
                    self.weights[i as usize].optimizer_data = self.optimizer_ffm.initial_data();
                }
            }

            for z in 0..self.attention_weights_len as usize {
                self.attention_weights[z].weight = 1.0; // We start with attention doing nothing
//                self.attention_weights[z].optimizer_data = self.optimizer_attention.initial_data();
            }
            /*println!("Enter limit: ");
                let mut line = String::new();
                let limit = std::io::stdin().read_line(&mut line).unwrap();
                let line = line[0..line.len() - 1].to_string();
                let limitf = line.parse().unwrap();
                */
            /*
            let filename = "ffm_attention_weights.bin.in";
            if path::Path::new(&filename).exists() {
                println!("Loading primary ffm attention weights from file: {}, len: {}", filename, self.attention_weights.len());
                let mut attention_weights =vec![WeightAndOptimizerData::<L>{weight:0.0, optimizer_data: self.optimizer_attention.initial_data()}; self.attention_weights.len() as usize];
                let mut input_bufreader = io::BufReader::new(fs::File::open(filename).unwrap());
                block_helpers::read_weights_from_buf(&mut attention_weights, &mut input_bufreader).unwrap();
                let limitf = 0.0;
                //println!("Truncating at {}", limitf);
                  
                for z in 0..self.attention_weights.len() as usize {
                    self.attention_weights[z].optimizer_data = attention_weights[z].optimizer_data;
                    self.attention_weights[z].weight = attention_weights[z].weight;
                    //self.attention_weights[z].optimizer_data = self.optimizer_attention.initial_data();
                }
            }
            let filename = "ffm_weights.bin.in";
            if path::Path::new(&filename).exists() {
                println!("Loading primary ffm weights from file: {}, len: {}", filename, self.weights.len());
                let mut weights =vec![WeightAndOptimizerData::<L>{weight:0.0, optimizer_data: self.optimizer_ffm.initial_data()}; self.weights.len() as usize];
                let mut input_bufreader = io::BufReader::new(fs::File::open(filename).unwrap());
                block_helpers::read_weights_from_buf(&mut weights, &mut input_bufreader).unwrap();
                
                for z in 0..self.weights.len() as usize {
//                    self.weights[z].optimizer_data = self.optimizer_ffm.initial_data();
                    self.weights[z].optimizer_data = weights[z].optimizer_data;
                    self.weights[z].weight = weights[z].weight;
                }
            }*/
        }
    }


    #[inline(always)]
    fn forward_backward(&mut self, 
                        further_blocks: &mut [Box<dyn BlockTrait>], 
                        wsum_input: f32, 
                        fb: &feature_buffer::FeatureBuffer, 
                        update:bool) -> (f32, f32) {
        let mut wsum = 0.0;
        let local_data_ffm_len = fb.ffm_buffer.len() * (self.ffm_k * fb.ffm_fields_count) as usize;
        unsafe {
            macro_rules! core_macro {
                (
                $local_ffm_derivatives:ident
                ) => {
                    let mut local_ffm_derivatives = $local_ffm_derivatives;
                     //   let mut local_ffm_derivatives = &mut $local_ffm_derivatives;
                            
                    let ffm_weights = &mut self.weights;
                    let fc = (fb.ffm_fields_count  * self.ffm_k) as usize;
                    let mut contra_fields: [f32; FFM_CONTRA_BUF_LEN] = MaybeUninit::uninit().assume_init();
                    let mut attention_derivatives: [f32; FFM_CONTRA_BUF_LEN] = MaybeUninit::uninit().assume_init();
                    
                    let field_embedding_len = self.field_embedding_len;
                    specialize_k!(self.ffm_k, FFMK, wsumbuf, {
                        /* first prepare two things:
                        - transposed contra vectors in contra_fields - 
                            - for each vector we sum up all the features within a field
                            - and at the same time transpose it, so we can later directly multiply them with individual feature embeddings
                        - cache of gradients in local_ffm_derivatives 
                            - we will use these gradients later in backward pass
                        */
//                        _mm_prefetch(mem::transmute::<&f32, &i8>(&contra_fields.get_unchecked(fb.ffm_buffer.get_unchecked(0).contra_field_index as usize)), _MM_HINT_T0);
                        let mut ffm_buffer_index = 0;
                        for field_index in 0..fb.ffm_fields_count {
                            let field_index_ffmk = field_index * FFMK;
                            let offset = (field_index_ffmk * fb.ffm_fields_count) as usize;
                            // first we handle fields with no features
                            if ffm_buffer_index >= fb.ffm_buffer.len() ||
                                fb.ffm_buffer.get_unchecked(ffm_buffer_index).contra_field_index > field_index_ffmk {
                                let mut zfc:usize = field_index_ffmk as usize;
                                for z in 0..fb.ffm_fields_count {
                                    for k in 0..FFMK as usize{
                                        *contra_fields.get_unchecked_mut(zfc + k) = 0.0;
                                    }
                                    zfc += fc;
                                }                                
                                continue;
                            } 
                            let mut feature_num = 0;
                            while ffm_buffer_index < fb.ffm_buffer.len() && fb.ffm_buffer.get_unchecked(ffm_buffer_index).contra_field_index == field_index_ffmk {
                                _mm_prefetch(mem::transmute::<&f32, &i8>(&ffm_weights.get_unchecked(fb.ffm_buffer.get_unchecked(ffm_buffer_index+1).hash as usize).weight), _MM_HINT_T0);
                                let left_hash = fb.ffm_buffer.get_unchecked(ffm_buffer_index);
                                let mut addr = left_hash.hash as usize;
                                let mut zfc:usize = field_index_ffmk as usize;
                                
                                specialize_value_f32!(left_hash.value, 1.0f32, LEFT_HASH_VALUE, {
                                    if feature_num == 0 {
                                        for z in 0..fb.ffm_fields_count {
                                            _mm_prefetch(mem::transmute::<&f32, &i8>(&ffm_weights.get_unchecked(addr + FFMK as usize).weight), _MM_HINT_T0);
                                            for k in 0..FFMK as usize{
                                                *contra_fields.get_unchecked_mut(zfc + k) = ffm_weights.get_unchecked(addr + k).weight * LEFT_HASH_VALUE;
                                            }
                                            zfc += fc;
                                            addr += FFMK as usize
                                        }
                                    } else {
                                        for z in 0..fb.ffm_fields_count {
                                            _mm_prefetch(mem::transmute::<&f32, &i8>(&ffm_weights.get_unchecked(addr + FFMK as usize).weight), _MM_HINT_T0);
                                            for k in 0..FFMK as usize{
                                                *contra_fields.get_unchecked_mut(zfc + k) += ffm_weights.get_unchecked(addr + k).weight * LEFT_HASH_VALUE;
                                            }
                                            zfc += fc;
                                            addr += FFMK as usize
                                        }
                                    }
                                });
                                ffm_buffer_index += 1;
                                feature_num += 1;
                            }
                        }
                        for z in 0..self.attention_weights_len as usize {
                            attention_derivatives[z] = 0.0;
                        }
                        let mut ffm_values_offset = 0;
                        for (i, left_hash) in fb.ffm_buffer.iter().enumerate() {
                            let mut contra_offset = (left_hash.contra_field_index * fb.ffm_fields_count) as usize;
                            let contra_pure_index = contra_offset / FFMK as usize; // super not nice, but division gets optimized away
                            let mut vv = 0;
                            let left_hash_value = left_hash.value;
                            let left_hash_contra_field_index = left_hash.contra_field_index;
                            let left_hash_hash = left_hash.hash as usize;
                            //let LEFT_HASH_VALUE = left_hash_value;
                            specialize_value_f32!(left_hash_value, 1.0_f32, LEFT_HASH_VALUE, {
                              for z in 0..fb.ffm_fields_count as usize {
                                  let attention = self.attention_weights.get_unchecked(z + contra_pure_index).weight;
                                  if vv == left_hash_contra_field_index as usize {
                                      for k in 0..FFMK as usize {
                                          let ffm_weight = ffm_weights.get_unchecked(left_hash_hash + vv + k).weight;
                                          let contra_weight = *contra_fields.get_unchecked(contra_offset + vv + k) - ffm_weight * LEFT_HASH_VALUE;
                                          let gradient =  LEFT_HASH_VALUE * contra_weight;
                                          let gradient2 = gradient * attention;
                                          *attention_derivatives.get_unchecked_mut(z + contra_pure_index) += gradient * ffm_weight;
                                          *local_ffm_derivatives.get_unchecked_mut(ffm_values_offset + k) = gradient2;
                                          *wsumbuf.get_unchecked_mut(k) += ffm_weight * gradient2;
                                      }
                                  } else {
                                      for k in 0..FFMK as usize {
                                          let ffm_weight = ffm_weights.get_unchecked(left_hash_hash + vv + k).weight;
                                          let contra_weight = *contra_fields.get_unchecked(contra_offset + vv + k);
                                          let gradient =  LEFT_HASH_VALUE * contra_weight;
                                          let gradient2 = gradient * attention;
                                          *attention_derivatives.get_unchecked_mut(z + contra_pure_index) += gradient * ffm_weight;
                                          *local_ffm_derivatives.get_unchecked_mut(ffm_values_offset + k) = gradient2;
                                          *wsumbuf.get_unchecked_mut(k) += ffm_weight * gradient2;
                                      }
                                  }
                                  vv += FFMK as usize;
                                  //left_hash_hash += FFMK as usize;
                                  //contra_offset += FFMK as usize;
                                  ffm_values_offset += FFMK as usize;
                              }
                            }); // End of macro specialize_1f32! for LEFT_HASH_VALUE
                        }    
                        for k in 0..FFMK as usize {
                            wsum += wsumbuf[k];
                        }
                        wsum *= 0.5;
                    });
                        
                    let (next_regressor, further_blocks) = further_blocks.split_at_mut(1);
                    let (prediction_probability, general_gradient) = next_regressor[0].forward_backward(further_blocks, wsum + wsum_input, fb, update);
                    
                    if update {
                        let mut local_index: usize = 0;
                        for left_hash in &fb.ffm_buffer {
                            let mut feature_index = left_hash.hash as usize;
                            for j in 0..fc as usize {
                                let feature_value = *local_ffm_derivatives.get_unchecked(local_index);
                                let gradient = general_gradient * feature_value;
                                let update_scale = self.optimizer_ffm.calculate_update(gradient, &mut ffm_weights.get_unchecked_mut(feature_index).optimizer_data);
                                let update = gradient * update_scale;
                                ffm_weights.get_unchecked_mut(feature_index).weight += update;
                                local_index += 1;
                                feature_index += 1;
                            }
                        }
                        
                        // Update attention
                        if fb.example_number < AFFM_FOR {
                    
                        specialize_value_f32!(self.attention_snap_to_zero, 0.0, ATTENTION_SNAP_TO_ZERO, {
                            specialize_value_f32!(self.attention_l2, 0.0, ATTENTION_L2, {
                                for z in 0..self.attention_weights_len as usize {
                                    let feature_value = attention_derivatives.get_unchecked(z);
                                    let gradient = general_gradient * feature_value;
                                    let update_scale = self.optimizer_attention.calculate_update(gradient, &mut self.attention_weights.get_unchecked_mut(z).optimizer_data);
                                    let update = gradient * update_scale;
                                    let mut oldweight = self.attention_weights.get_unchecked(z).weight;
                                    if ATTENTION_L2 != 0.0 && gradient != 0.0 { // only update if the weight was present
                                        oldweight -= oldweight * (ATTENTION_L2 * update_scale);
                                    }
                                    
                                    oldweight += update;
                                    if ATTENTION_SNAP_TO_ZERO != 0.0 {
                                        if oldweight < ATTENTION_SNAP_TO_ZERO {
                                            oldweight = 0.0;
                                        }
                                    }
                                    
                                    /*if oldweight > 1.95 {
                                      oldweight = 2.0;
                                    }*/
                                    if oldweight > 1.2 {
                                        oldweight = 1.2;
                                    }

                                    
                                    self.attention_weights.get_unchecked_mut(z).weight = oldweight;
                                }
                            });
                        });
                        }
                    }
                    // The only exit point
                    return (prediction_probability, general_gradient)
                }
            }; // End of macro
            

            if local_data_ffm_len < FFM_STACK_BUF_LEN {
                // Fast-path - using on-stack data structures
                let mut local_ffm_derivatives: [f32; FFM_STACK_BUF_LEN as usize] = MaybeUninit::uninit().assume_init();//[0.0; FFM_STACK_BUF_LEN as usize];
                core_macro!(local_ffm_derivatives);

            } else {
                // Slow-path - using heap data structures
                if local_data_ffm_len > self.local_ffm_derivatives.len() {
                    self.local_ffm_derivatives.reserve(local_data_ffm_len - self.local_ffm_derivatives.len() + 1024);
                }
                let mut local_ffm_derivatives = &mut self.local_ffm_derivatives;
            
                core_macro!(local_ffm_derivatives);
            }             
        } // unsafe end
    }
    
    fn forward(&self, further_blocks: &[Box<dyn BlockTrait>], wsum_input: f32, fb: &feature_buffer::FeatureBuffer) -> f32 {
        let mut wsum:f32 = 0.0;
        unsafe {
            let ffm_weights = &self.weights;
            if true {
                _mm_prefetch(mem::transmute::<&f32, &i8>(&ffm_weights.get_unchecked(fb.ffm_buffer.get_unchecked(0).hash as usize).weight), _MM_HINT_T0);
                let field_embedding_len = self.field_embedding_len as usize;
                let mut contra_fields: [f32; FFM_STACK_BUF_LEN] = MaybeUninit::uninit().assume_init();

                specialize_k!(self.ffm_k, FFMK, wsumbuf, {
                    /* We first prepare "contra_fields" or collapsed field embeddings, where we sum all individual feature embeddings
                       We need to be careful to:
                       - handle fields with zero features present
                       - handle values on diagonal - we want to be able to exclude self-interactions later (we pre-substract from wsum)
                       - optimize for just copying the embedding over when looking at first feature of the field, and add embeddings for the rest
                       - optiize for very common case of value of the feature being 1.0 - avoid multiplications
                       - 
                    */ 
                    
                    let mut ffm_buffer_index = 0;
                    for field_index in 0..fb.ffm_fields_count {
                        let field_index_ffmk = field_index * FFMK;
                        let offset = (field_index_ffmk * fb.ffm_fields_count) as usize;
                        // first we handle fields with no features
                        if ffm_buffer_index >= fb.ffm_buffer.len() ||
                            fb.ffm_buffer.get_unchecked(ffm_buffer_index).contra_field_index > field_index_ffmk {
                            for z in 0..field_embedding_len as usize { // first time we see this field - just overwrite
                                *contra_fields.get_unchecked_mut(offset + z) = 0.0;
                            }
                            continue;
                        } 
                        let mut feature_num = 0;
                        while ffm_buffer_index < fb.ffm_buffer.len() && fb.ffm_buffer.get_unchecked(ffm_buffer_index).contra_field_index == field_index_ffmk {
                            _mm_prefetch(mem::transmute::<&f32, &i8>(&ffm_weights.get_unchecked(fb.ffm_buffer.get_unchecked(ffm_buffer_index+1).hash as usize).weight), _MM_HINT_T0);
                            let left_hash = fb.ffm_buffer.get_unchecked(ffm_buffer_index);
                            let left_hash_hash = left_hash.hash as usize;
                            let left_hash_value = left_hash.value;
                            specialize_value_f32!(left_hash_value, 1.0_f32, LEFT_HASH_VALUE, {
                                if feature_num == 0 {
                                    for z in 0..field_embedding_len { // first feature of the field - just overwrite
                                        *contra_fields.get_unchecked_mut(offset + z) = ffm_weights.get_unchecked(left_hash_hash + z).weight * LEFT_HASH_VALUE;
                                    }
                                } else {
                                    for z in 0..field_embedding_len { // additional features of the field - addition
                                        *contra_fields.get_unchecked_mut(offset + z) += ffm_weights.get_unchecked(left_hash_hash + z).weight * LEFT_HASH_VALUE;
                                    }
                                }
                                let attention_value = self.attention_weights.get_unchecked((field_index * fb.ffm_fields_count + field_index) as usize).weight;
                                let vv = SQRT_OF_ONE_HALF * LEFT_HASH_VALUE;     // To avoid one additional multiplication, we square root 0.5 into vv
                                for k in 0..FFMK as usize {
                                    let ss = ffm_weights.get_unchecked(left_hash_hash + field_index_ffmk as usize + k).weight * vv;
                                    *wsumbuf.get_unchecked_mut(k) -= ss * ss * attention_value;
                                }
                            });
                            ffm_buffer_index += 1;
                            feature_num += 1;
                        }
                    }
                    

                    for f1 in 0..fb.ffm_fields_count as usize {
                        let f1_offset = f1 * field_embedding_len as usize;
                        let f1_ffmk = f1 * FFMK as usize;
                        let mut f2_offset_ffmk = f1_offset + f1_ffmk;
                        let mut f1_offset_ffmk = f1_offset + f1_ffmk;
                        // This is self-interaction
                        let f1_attention_offset = f1 * fb.ffm_fields_count as usize;
                        /*let f2_attention_offset = f2 * fb.ffm_fields_count as usize;
                        println!("A: {} {}", self.attention_weights.get_unchecked(f1 + f1_attention_offset).weight, self.attention_weights.get_unchecked(f2 + f2_attention_offset).weight);*/
                        let attention_value = self.attention_weights.get_unchecked(f1 + f1_attention_offset).weight;
                        for k in 0..FFMK as usize { 
                            let v = contra_fields.get_unchecked(f1_offset_ffmk + k);
                            *wsumbuf.get_unchecked_mut(k) += v * v * 0.5 * attention_value;
                        }

                        for f2 in f1+1..fb.ffm_fields_count as usize {
                            let attention_value = self.attention_weights.get_unchecked(f2 + f1_attention_offset).weight;
                            f2_offset_ffmk += field_embedding_len as usize;
                            f1_offset_ffmk += FFMK as usize;
                            assert_eq!(f1_offset_ffmk, f1 * field_embedding_len + f2 * FFMK as usize);
                            assert_eq!(f2_offset_ffmk, f2 * field_embedding_len + f1 * FFMK as usize);
                            for k in 0..FFMK {
                                *wsumbuf.get_unchecked_mut(k as usize) += 
                                        contra_fields.get_unchecked(f1_offset_ffmk + k as usize) * 
                                        contra_fields.get_unchecked(f2_offset_ffmk + k as usize) * 
                                        attention_value;
                            }
                        }
                        
                    }
                    for k in 0..FFMK as usize {
                        wsum += wsumbuf[k];
                    }
                });
            } else {
                // Old straight-forward method. As soon as we have multiple feature values per field, it is slower
                let ffm_weights = &self.weights;
                specialize_k!(self.ffm_k, FFMK, wsumbuf, {                        
                    for (i, left_hash) in fb.ffm_buffer.iter().enumerate() {
                        for right_hash in fb.ffm_buffer.get_unchecked(i+1 ..).iter() {
                            //if left_hash.contra_field_index == right_hash.contra_field_index {
                            //    continue	// not combining within a field
                            //}
                            let f1 = left_hash.contra_field_index / FFMK;
                            let f2 = right_hash.contra_field_index / FFMK;
                            let attention_value = self.attention_weights.get_unchecked((f2 + f1 * fb.ffm_fields_count) as usize).weight;
                            let joint_value = left_hash.value * right_hash.value;
                            let lindex = (left_hash.hash + right_hash.contra_field_index) as u32;
                            let rindex = (right_hash.hash + left_hash.contra_field_index) as u32;
                            for k in 0..FFMK {
                                let left_hash_weight  = ffm_weights.get_unchecked((lindex+k) as usize).weight;
                                let right_hash_weight = ffm_weights.get_unchecked((rindex+k) as usize).weight;
                                *wsumbuf.get_unchecked_mut(k as usize) += left_hash_weight * right_hash_weight * joint_value * attention_value;  
                            }
                        }
                    
                    }
                    for k in 0..FFMK as usize {
                        wsum += wsumbuf[k];
                    }
                });
            }
        }
        let (next_regressor, further_blocks) = further_blocks.split_at(1);
        let prediction_probability = next_regressor[0].forward(further_blocks, wsum + wsum_input, fb);
        prediction_probability         
                 
    }
    
    fn get_serialized_len(&self) -> usize {
        return (self.ffm_weights_len + self.attention_weights_len) as usize;
    }

    fn read_weights_from_buf(&mut self, input_bufreader: &mut dyn io::Read) -> Result<(), Box<dyn Error>> {
        block_helpers::read_weights_from_buf(&mut self.weights, input_bufreader).unwrap();
        block_helpers::read_weights_from_buf(&mut self.attention_weights, input_bufreader).unwrap();
        Ok(())
    }

    fn write_weights_to_buf(&self, output_bufwriter: &mut dyn io::Write) -> Result<(), Box<dyn Error>> {
        block_helpers::write_weights_to_buf(&self.weights, output_bufwriter).unwrap();
        block_helpers::write_weights_to_buf(&self.attention_weights, output_bufwriter).unwrap();
        Ok(())
    }

    fn read_weights_from_buf_into_forward_only(&self, input_bufreader: &mut dyn io::Read, forward: &mut Box<dyn BlockTrait>) -> Result<(), Box<dyn Error>> {
        let mut forward = forward.as_any().downcast_mut::<BlockAFFM<optimizer::OptimizerSGD>>().unwrap();
        block_helpers::read_weights_only_from_buf2::<L>(self.ffm_weights_len as usize, &mut forward.weights, input_bufreader).unwrap();
        block_helpers::read_weights_only_from_buf2::<L>(self.attention_weights_len as usize, &mut forward.attention_weights, input_bufreader).unwrap();
        Ok(())
    }

    /// Sets internal state of weights based on some completely object-dependent parameters
    fn testing_set_weights(&mut self, aa: i32, bb: i32, index: usize, w: &[f32]) -> Result<(), Box<dyn Error>> {
        self.weights[index].weight = w[0];
        self.weights[index].optimizer_data = self.optimizer_ffm.initial_data();
        Ok(())
    }

    fn debug_output(&mut self, mi: &model_instance::ModelInstance, aa: i32) {
        let field_count = mi.ffm_fields.len() as usize;
        for f1 in 0..field_count {
            println!("Combining: {} with field", mi.audit_aux_data.as_ref().unwrap().field_index_to_string[&(f1 as u32)]);
            for f2 in 0..field_count {
                print!("{:.2}  ", self.attention_weights[(f2+f1*field_count) as usize].weight);
                print!("     => {}, squared gradient acc: {}", mi.audit_aux_data.as_ref().unwrap().field_index_to_string[&(f2 as u32)], 
                                                              L::format_data(&self.attention_weights[(f2+f1*field_count) as usize].optimizer_data));
                println!(" ");
            }
        }
        if aa == 1 {
/*            println!("Outputting files");
            let filename = "ffm_attention_weights.bin";
            let output_bufwriter = &mut io::BufWriter::new(fs::File::create(filename).expect(format!("Cannot open {} to save regressor to", filename).as_str()));
            block_helpers::write_weights_to_buf(&self.attention_weights, output_bufwriter).unwrap();
            let filename = "ffm_weights.bin";
            let output_bufwriter = &mut io::BufWriter::new(fs::File::create(filename).expect(format!("Cannot open {} to save regressor to", filename).as_str()));
            block_helpers::write_weights_to_buf(&self.weights, output_bufwriter).unwrap();
*/
/*
            let filename = "ffm_attention_weights.bin.in";
            if path::Path::new(&filename).exists() {
                println!("Loading secondary ffm attention weights from file: {}, len: {}", filename, self.attention_weights.len());
                let mut attention_weights =vec![WeightAndOptimizerData::<L>{weight:0.0, optimizer_data: self.optimizer_attention.initial_data()}; self.attention_weights.len() as usize];
                let mut input_bufreader = io::BufReader::new(fs::File::open(filename).unwrap());
                block_helpers::read_weights_from_buf(&mut attention_weights, &mut input_bufreader).unwrap();
                let limitf = 0.0;
                //println!("Truncating at {}", limitf);
                  
                for z in 0..self.attention_weights.len() as usize {
                    self.attention_weights[z].optimizer_data = attention_weights[z].optimizer_data;
             //       self.attention_weights[z].weight = attention_weights[z].weight;
                    //self.attention_weights[z].optimizer_data = self.optimizer_attention.initial_data();
                }
            }
            let filename = "ffm_weights.bin.in";
            if path::Path::new(&filename).exists() {
                println!("Loading secondary ffm weights from file: {}, len: {}", filename, self.weights.len());
                let mut weights =vec![WeightAndOptimizerData::<L>{weight:0.0, optimizer_data: self.optimizer_ffm.initial_data()}; self.weights.len() as usize];
                let mut input_bufreader = io::BufReader::new(fs::File::open(filename).unwrap());
                block_helpers::read_weights_from_buf(&mut weights, &mut input_bufreader).unwrap();
                
                for z in 0..self.weights.len() as usize {
//                    self.weights[z].optimizer_data = self.optimizer_ffm.initial_data();
                    self.weights[z].optimizer_data = weights[z].optimizer_data;
                    //self.weights[z].weight = weights[z].weight;
                }
            }
*/

        }

    }

}




mod tests {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;
    use crate::block_loss_functions::BlockSigmoid;
    use crate::feature_buffer;
    use crate::feature_buffer::HashAndValueAndSeq;
    use crate::vwmap;
    use block_helpers::{slearn, spredict};

    use crate::assert_epsilon;


    fn ffm_vec(v:Vec<feature_buffer::HashAndValueAndSeq>, ffm_fields_count: u32) -> feature_buffer::FeatureBuffer {
        feature_buffer::FeatureBuffer {
                    label: 0.0,
                    example_importance: 1.0,
                    example_number: 0,
                    lr_buffer: Vec::new(),
                    ffm_buffer: v,
                    ffm_fields_count: ffm_fields_count,
        }
    }

    fn ffm_init<T:OptimizerTrait + 'static>(block_ffm: &mut Box<dyn BlockTrait>) -> () {
        let mut block_ffm = block_ffm.as_any().downcast_mut::<BlockAFFM<T>>().unwrap();
        
        for i in 0..block_ffm.weights.len() {
            block_ffm.weights[i].weight = 1.0;
            block_ffm.weights[i].optimizer_data = block_ffm.optimizer_ffm.initial_data();
        }
        for i in 0..block_ffm.attention_weights.len() {
            block_ffm.attention_weights[i].weight = 1.0;
            block_ffm.attention_weights[i].optimizer_data = block_ffm.optimizer_attention.initial_data();
        }
    }

    #[test]
    fn test_affm_k1() {
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.ffm_learning_rate = 0.1;
        mi.power_t = 0.0;
        mi.ffm_power_t = 0.0;
        mi.bit_precision = 18;
        mi.ffm_k = 1;
        mi.ffm_bit_precision = 18;
        mi.ffm_fields = vec![vec![], vec![]]; // This isn't really used
        mi.attention = true;
        let mut lossf = BlockSigmoid::new_without_weights(&mi).unwrap();
        
        // Nothing can be learned from a single field in FFMs
        let mut re = BlockAFFM::<optimizer::OptimizerAdagradLUT>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);

        let fb = ffm_vec(vec![HashAndValueAndSeq{hash:1, value: 1.0, contra_field_index: 0}], 
                        1); // saying we have 1 field isn't entirely correct
        assert_epsilon!(spredict(&mut re, &mut lossf, &fb, true), 0.5);
        assert_epsilon!(slearn  (&mut re, &mut lossf, &fb, true), 0.5);

        // With two fields, things start to happen
        // Since fields depend on initial randomization, these tests are ... peculiar.
        let mut re = BlockAFFM::<optimizer::OptimizerAdagradFlex>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);

        ffm_init::<optimizer::OptimizerAdagradFlex>(&mut re);
        let fb = ffm_vec(vec![
                                  HashAndValueAndSeq{hash:1, value: 1.0, contra_field_index: 0},
                                  HashAndValueAndSeq{hash:100, value: 1.0, contra_field_index: mi.ffm_k}
                                  ], 2);
                                  
        assert_epsilon!(spredict(&mut re, &mut lossf, &fb, true), 0.7310586);      
        assert_eq!(slearn  (&mut re, &mut lossf, &fb, true), 0.7310586); 
   
        assert_epsilon!(spredict(&mut re, &mut lossf, &fb, true), 0.69055194);
        assert_eq!(slearn  (&mut re, &mut lossf, &fb, true), 0.69055194);

        // Two fields, use values
        let mut re = BlockAFFM::<optimizer::OptimizerAdagradLUT>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);

        ffm_init::<optimizer::OptimizerAdagradLUT>(&mut re);
        let fb = ffm_vec(vec![
                                  HashAndValueAndSeq{hash:1, value: 2.0, contra_field_index: 0},
                                  HashAndValueAndSeq{hash:100, value: 2.0, contra_field_index: mi.ffm_k * 1}
                                  ], 2);
        assert_eq!(spredict(&mut re, &mut lossf, &fb, true), 0.98201376);
        assert_eq!(slearn(&mut re, &mut lossf, &fb, true), 0.98201376);
        assert_eq!(spredict(&mut re, &mut lossf, &fb, true), 0.76625353);
        assert_eq!(slearn(&mut re, &mut lossf, &fb, true), 0.76625353);
        
    }


    #[test]
    fn test_affm_k4() {
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.ffm_learning_rate = 0.1;
        mi.power_t = 0.0;
        mi.ffm_power_t = 0.0;
        mi.ffm_k = 4;
        mi.ffm_bit_precision = 18;
        mi.ffm_fields = vec![vec![], vec![]]; // This isn't really used
        let mut lossf = BlockSigmoid::new_without_weights(&mi).unwrap();
        
        // Nothing can be learned from a single field in FFMs
        let mut re = BlockAFFM::<optimizer::OptimizerAdagradLUT>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);

        let fb = ffm_vec(vec![HashAndValueAndSeq{hash:1, value: 1.0, contra_field_index: 0}], 4);
        assert_eq!(spredict(&mut re, &mut lossf, &fb, true), 0.5);
        assert_eq!(slearn(&mut re, &mut lossf, &fb, true), 0.5);
        assert_eq!(spredict(&mut re, &mut lossf, &fb, true), 0.5);
        assert_eq!(slearn(&mut re, &mut lossf, &fb, true), 0.5);

        // With two fields, things start to happen
        // Since fields depend on initial randomization, these tests are ... peculiar.
        let mut re = BlockAFFM::<optimizer::OptimizerAdagradFlex>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);

        ffm_init::<optimizer::OptimizerAdagradFlex>(&mut re);
        let fb = ffm_vec(vec![
                                  HashAndValueAndSeq{hash:1, value: 1.0, contra_field_index: 0},
                                  HashAndValueAndSeq{hash:100, value: 1.0, contra_field_index: mi.ffm_k * 1}
                                  ], 2);
        assert_eq!(spredict(&mut re, &mut lossf, &fb, true), 0.98201376); 
        assert_eq!(slearn  (&mut re, &mut lossf, &fb, true), 0.98201376); 
        assert_eq!(spredict(&mut re, &mut lossf, &fb, true), 0.9320294);
        assert_eq!(slearn  (&mut re, &mut lossf, &fb, true), 0.9320294);
        // Two fields, use values
        let mut re = BlockAFFM::<optimizer::OptimizerAdagradLUT>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);

        ffm_init::<optimizer::OptimizerAdagradLUT>(&mut re);
        let fb = ffm_vec(vec![
                                  HashAndValueAndSeq{hash:1, value: 2.0, contra_field_index: 0},
                                  HashAndValueAndSeq{hash:100, value: 2.0, contra_field_index: mi.ffm_k * 1}
                                  ], 2);
        assert_eq!(spredict(&mut re, &mut lossf, &fb, true), 0.9999999);
        assert_eq!(slearn(&mut re, &mut lossf, &fb, true), 0.9999999);
        assert_eq!(spredict(&mut re, &mut lossf, &fb, true), 0.9689196);
        assert_eq!(slearn(&mut re, &mut lossf, &fb, true), 0.9689196);
    }


    #[test]
    fn test_affm_multivalue() {
        let vw_map_string = r#"
A,featureA
B,featureB
"#;
        let vw = vwmap::VwNamespaceMap::new(vw_map_string, ("".to_string(), 0)).unwrap();
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();
        mi.learning_rate = 0.1;
        mi.power_t = 0.0;
        mi.ffm_k = 1;
        mi.ffm_bit_precision = 18;
        mi.ffm_power_t = 0.0;
        mi.ffm_learning_rate = 0.1;
        mi.ffm_fields = vec![vec![],vec![]]; 
        mi.optimizer = model_instance::Optimizer::Adagrad;
        mi.fastmath = false;
        let mut lossf = BlockSigmoid::new_without_weights(&mi).unwrap();

        let mut re = BlockAFFM::<optimizer::OptimizerAdagradLUT>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);
        let mut p: f32;

        ffm_init::<optimizer::OptimizerAdagradLUT>(&mut re);
        let fbuf = &ffm_vec(vec![
                                  HashAndValueAndSeq{hash:1, value: 1.0, contra_field_index: 0},
                                  HashAndValueAndSeq{hash:3 * 1000, value: 1.0, contra_field_index: 0},
                                  HashAndValueAndSeq{hash:100, value: 2.0, contra_field_index: mi.ffm_k * 1}
                                  ], 2);
                                  
        assert_epsilon!(spredict(&mut re, &mut lossf, &fbuf, true), 0.9933072);
        assert_eq!(slearn(&mut re, &mut lossf, &fbuf, true), 0.9933072);
        assert_epsilon!(slearn(&mut re, &mut lossf, &fbuf, false), 0.90496447);
        assert_epsilon!(spredict(&mut re, &mut lossf, &fbuf, false), 0.90496447);
    }

    #[test]
    fn test_affm_multivalue_k4_nonzero_powert() {
        let vw_map_string = r#"
A,featureA
B,featureB
"#;
        let vw = vwmap::VwNamespaceMap::new(vw_map_string, ("".to_string(), 0)).unwrap();
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();
        mi.ffm_k = 4;
        mi.ffm_bit_precision = 18;
        mi.ffm_fields = vec![vec![],vec![]]; 
        mi.optimizer = model_instance::Optimizer::Adagrad;
        mi.fastmath = false;
        let mut lossf = BlockSigmoid::new_without_weights(&mi).unwrap();

        let mut re = BlockAFFM::<optimizer::OptimizerAdagradLUT>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);
        ffm_init::<optimizer::OptimizerAdagradLUT>(&mut re);
        let fbuf = &ffm_vec(vec![
                                  HashAndValueAndSeq{hash:1, value: 1.0, contra_field_index: 0},
                                  HashAndValueAndSeq{hash:3 * 1000, value: 1.0, contra_field_index: 0},
                                  HashAndValueAndSeq{hash:100, value: 2.0, contra_field_index: mi.ffm_k * 1}
                                  ], 2);

        assert_eq!(spredict(&mut re, &mut lossf, &fbuf, true), 1.0);
        assert_eq!(slearn(&mut re, &mut lossf, &fbuf, true), 1.0);
        
        assert_eq!(spredict(&mut re, &mut lossf, &fbuf, false), 0.9654269);
        assert_eq!(slearn(&mut re, &mut lossf, &fbuf, false), 0.9654269);
        assert_eq!(slearn(&mut re, &mut lossf, &fbuf, false), 0.9654269);
    }

    #[test]
    fn test_affm_missing_field() {
        // This test is useful to check if we don't by accient forget to initialize any of the collapsed
        // embeddings for the field, when field has no instances of a feature in it
        // We do by having three-field situation where only the middle field has features
        let mut mi = model_instance::ModelInstance::new_empty().unwrap();        
        mi.learning_rate = 0.1;
        mi.ffm_learning_rate = 0.1;
        mi.power_t = 0.0;
        mi.ffm_power_t = 0.0;
        mi.bit_precision = 18;
        mi.ffm_k = 1;
        mi.ffm_bit_precision = 18;
        mi.ffm_fields = vec![vec![], vec![], vec![]]; // This isn't really used
        let mut lossf = BlockSigmoid::new_without_weights(&mi).unwrap();
        
        // Nothing can be learned from a single field in FFMs
        let mut re = BlockAFFM::<optimizer::OptimizerAdagradLUT>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);


        // With two fields, things start to happen
        // Since fields depend on initial randomization, these tests are ... peculiar.
        let mut re = BlockAFFM::<optimizer::OptimizerAdagradFlex>::new_without_weights(&mi).unwrap();
        re.allocate_and_init_weights(&mi);

        ffm_init::<optimizer::OptimizerAdagradFlex>(&mut re);
        let fb = ffm_vec(vec![
                                  HashAndValueAndSeq{hash:1, value: 1.0, contra_field_index: 0},
                                  HashAndValueAndSeq{hash:5, value: 1.0, contra_field_index: mi.ffm_k * 1},
                                  HashAndValueAndSeq{hash:100, value: 1.0, contra_field_index: mi.ffm_k * 2}
                                  ], 3);
        assert_epsilon!(spredict(&mut re, &mut lossf, &fb, true), 0.95257413); 
        assert_eq!(slearn  (&mut re, &mut lossf, &fb, false), 0.95257413); 

        // here we intentionally have just the middle field
        let fb = ffm_vec(vec![HashAndValueAndSeq{hash:5, value: 1.0, contra_field_index: mi.ffm_k * 1}], 3);
        assert_eq!(spredict(&mut re, &mut lossf, &fb, true), 0.5);
        assert_eq!(slearn  (&mut re, &mut lossf, &fb, true), 0.5);

    }
}


