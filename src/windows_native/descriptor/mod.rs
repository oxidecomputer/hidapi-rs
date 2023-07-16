mod typedefs;

use std::collections::HashMap;
use std::ffi::c_void;
use std::iter::once;
use std::ptr::addr_of;
use std::slice;
use crate::ensure;
use crate::windows_native::descriptor::typedefs::{HidpPreparsedData, LinkCollectionNode, ReportType};
use crate::windows_native::error::{WinError, WinResult};
use crate::windows_native::hid::PreparsedData;

#[derive(Default, Copy, Clone, Eq, PartialEq)]
struct BitRange {
    first_bit: Option<u16>,
    last_bit: Option<u16>
}

const INVALID_DATA: WinResult<usize> = Err(WinError::InvalidPreparsedData);

pub fn get_descriptor(pp_data: &PreparsedData, buf: &mut [u8]) -> WinResult<usize> {
    //let mut out = buf;
    unsafe {
        let header: *const HidpPreparsedData = pp_data.as_ptr() as _;
        // Check if MagicKey is correct, to ensure that pp_data points to an valid preparse data structure
        ensure!(&(*header).magic_key == b"HidP KDR", INVALID_DATA);
        // Set pointer to the first node of link_collection_nodes
        let link_collection_nodes = {
            let ptr: *const LinkCollectionNode = ((addr_of!((*header).caps_info[0]) as *const c_void).offset((*header).first_byte_of_link_collection_array as isize)) as _;
            let len = (*header).number_link_collection_nodes as usize;
            slice::from_raw_parts(ptr, len)
        };

        // ****************************************************************************************************************************
        // Create lookup tables for the bit range of each report per collection (position of first bit and last bit in each collection)
        // coll_bit_range[COLLECTION_INDEX][REPORT_ID][INPUT/OUTPUT/FEATURE]
        // ****************************************************************************************************************************
        let mut coll_bit_range: HashMap<(u16, u16, ReportType), BitRange> = HashMap::new();
        for collection_node_idx in 0..((*header).number_link_collection_nodes) {
            for reportid_idx in 0..256 {
                for rt_idx in ReportType::values() {
                    coll_bit_range.insert((collection_node_idx, reportid_idx, rt_idx), BitRange::default());
                }
            }
        }

        for rt_idx in ReportType::values() {
            let caps_info = (*header).caps_info[rt_idx as usize];
            for caps_idx in caps_info.first_cap..caps_info.last_cap {
                let caps = (*header).caps[caps_idx as usize];
                let first_bit = (caps.byte_position - 1) * 8 + caps.bit_position as u16;
                let last_bit = first_bit + caps.report_size * caps.report_count - 1;
                let range = coll_bit_range.get_mut(&(caps.link_collection, caps.report_id as u16, rt_idx)).unwrap();
                range.first_bit = range.first_bit.into_iter().chain(once(first_bit)).min();
                range.last_bit = range.last_bit.into_iter().chain(once(last_bit)).max();
            }
        }

        // *************************************************************************
        // -Determine hierachy levels of each collections and store it in:
        //  coll_levels[COLLECTION_INDEX]
        // -Determine number of direct childs of each collections and store it in:
        //  coll_number_of_direct_childs[COLLECTION_INDEX]
        // *************************************************************************
        let mut max_coll_level = 0;
        let mut coll_levels = Vec::new();
        let mut coll_number_of_direct_childs = Vec::new();
        for _ in 0..((*header).number_link_collection_nodes) {
            coll_levels.push(-1);
            coll_number_of_direct_childs.push(0);
        }

        {
            let mut actual_coll_level = 0;
            let mut collection_node_idx = 0;
            while actual_coll_level >= 0 {
                coll_levels[collection_node_idx] = actual_coll_level;
                let node = link_collection_nodes[collection_node_idx];
                if node.number_of_children > 0 && coll_levels[node.first_child as usize] == -1 {
                    actual_coll_level += 1;
                    coll_levels[collection_node_idx] = actual_coll_level;
                    max_coll_level = max_coll_level.max(actual_coll_level);
                    coll_number_of_direct_childs[collection_node_idx] += 1;
                    collection_node_idx = node.first_child as usize;
                } else if node.next_sibling != 0 {
                    coll_number_of_direct_childs[node.parent as usize] += 1;
                    collection_node_idx = node.next_sibling as usize;
                } else {
                    actual_coll_level -= 1;
                    if actual_coll_level >= 0 {
                        collection_node_idx = node.parent as usize;
                    }
                }
            }
        }

        // *********************************************************************************
        // Propagate the bit range of each report from the child collections to their parent
        // and store the merged result for the parent
        // *********************************************************************************
        for actual_coll_level in (0..max_coll_level).rev() {
            for collection_node_idx in 0..link_collection_nodes.len() {
                if coll_levels[collection_node_idx] == actual_coll_level {
                    let mut child_idx = link_collection_nodes[collection_node_idx].first_child;
                    while child_idx != 0 {
                        for reportid_idx in 0..256 {
                            for rt_idx in ReportType::values() {
                                let child = coll_bit_range
                                    .get(&(child_idx, reportid_idx, rt_idx))
                                    .unwrap()
                                    .clone();
                                let parent = coll_bit_range
                                    .get_mut(&(collection_node_idx as u16, reportid_idx, rt_idx))
                                    .unwrap();
                                parent.first_bit = parent.first_bit.into_iter().chain(child.first_bit).min();
                                parent.last_bit = parent.last_bit.into_iter().chain(child.last_bit).max();
                                child_idx = link_collection_nodes[child_idx as usize].next_sibling;
                            }
                        }
                    }
                }
            }
        }

        // *************************************************************************************************
        // Determine child collection order of the whole hierachy, based on previously determined bit ranges
        // and store it this index coll_child_order[COLLECTION_INDEX][DIRECT_CHILD_INDEX]
        // *************************************************************************************************
        let mut coll_child_order: HashMap<(u16, u16), u16> = HashMap::new();
        {
            let mut coll_parsed_flag = vec![false; link_collection_nodes.len()];
            let mut actual_coll_level = 0;
            let mut collection_node_idx = 0;
            while actual_coll_level >= 0 {
                if coll_number_of_direct_childs[collection_node_idx] != 0 &&
                    !coll_parsed_flag[link_collection_nodes[collection_node_idx].first_child as usize] {
                    coll_parsed_flag[link_collection_nodes[collection_node_idx].first_child as usize] = true;

                    {
                        // Create list of child collection indices
                        // sorted reverse to the order returned to HidP_GetLinkCollectionNodeschild
                        // which seems to match teh original order, as long as no bit position needs to be considered
                        let mut child_idx = link_collection_nodes[collection_node_idx].first_child;
                        let mut child_count = coll_number_of_direct_childs[collection_node_idx] - 1;
                        coll_child_order.insert((collection_node_idx as u16, child_count as u16), child_idx);
                        while link_collection_nodes[child_idx as usize].next_sibling != 0 {
                            child_count -= 1;
                            child_idx = link_collection_nodes[child_idx as usize].next_sibling;
                            coll_child_order.insert((collection_node_idx as u16, child_count as u16), child_idx);
                        }
                    }

                    if coll_number_of_direct_childs[collection_node_idx] > 1 {
                        // Sort child collections indices by bit positions
                        for rt_idx in ReportType::values() {
                            for report_idx in 0..256 {
                                for child_idx in 1..coll_number_of_direct_childs[collection_node_idx] {
                                    // since the coll_bit_range array is not sorted, we need to reference the collection index in
                                    // our sorted coll_child_order array, and look up the corresponding bit ranges for comparing values to sort
                                    let prev_coll_idx = *coll_child_order
                                        .get(&(collection_node_idx as u16, (child_idx - 1) as u16))
                                        .unwrap();
                                    let cur_coll_idx = *coll_child_order
                                        .get(&(collection_node_idx as u16, child_idx as u16))
                                        .unwrap();
                                    let swap = coll_bit_range
                                        .get(&(prev_coll_idx, report_idx, rt_idx))
                                        .and_then(|prev| prev.first_bit)
                                        .zip(coll_bit_range
                                            .get(&(cur_coll_idx, report_idx, rt_idx))
                                            .and_then(|prev| prev.first_bit))
                                        .map_or(false, |(prev, cur)| prev > cur);
                                    if swap {
                                        coll_child_order.insert((collection_node_idx as u16, (child_idx - 1) as u16), cur_coll_idx);
                                        coll_child_order.insert((collection_node_idx as u16, child_idx as u16), prev_coll_idx);
                                    }
                                }
                            }
                        }
                    }
                    actual_coll_level += 1;
                    collection_node_idx = link_collection_nodes[collection_node_idx].first_child as usize;
                } else if link_collection_nodes[collection_node_idx].next_sibling != 0 {
                    collection_node_idx = link_collection_nodes[collection_node_idx].next_sibling as usize;
                } else {
                    actual_coll_level -= 1;
                    if actual_coll_level >= 0 {
                        collection_node_idx = link_collection_nodes[collection_node_idx].parent as usize;
                    }
                }
            }
        }



        // TODO Implement the rest
        // https://github.com/libusb/hidapi/blob/d0856c05cecbb1522c24fd2f1ed1e144b001f349/windows/hidapi_descriptor_reconstruct.c#L199
    }
    Ok(0)
}

