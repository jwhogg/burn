use std::{any::Any, collections::HashMap, sync::Arc};

use crate::{
    graph::{Graph, NodeID, NodeRef},
    ops::CheckpointingAction,
};
use std::fmt::Debug;

use super::state::{BackwardStates, State};

/// Definition of the forward function of a node, called during retropropagation only.
/// This is different from the normal forward function because it reads and writes from
/// the [InnerStates] map instead of having a clear function signature.
pub trait RetroForward: Debug + Send + Sync + 'static {
    fn forward(&self, states: &mut BackwardStates, out_node: NodeID);
}

#[derive(new, Default, Debug)]
/// Links [NodeID]s to their corresponding [RetroForward]
pub(crate) struct RetroForwards {
    map: HashMap<NodeID, Arc<dyn RetroForward>>,
}

impl RetroForwards {
    /// Executes the [RetroForward] for a given [NodeID] if the node's
    /// [State] is [State::Recompute], otherwise does nothing.
    fn execute_retro_forward(&mut self, node_id: NodeID, backward_states: &mut BackwardStates) {
        let n_required = match backward_states.get_state_ref(&node_id).unwrap() {
            State::Recompute { n_required } => *n_required,
            State::Computed {
                state_content: _,
                n_required: _,
            } => return,
        };

        let retro_forward = self.map.remove(&node_id).unwrap();
        retro_forward.forward(backward_states, node_id.clone());
        if n_required > 1 {
            self.map.insert(node_id, retro_forward);
        }
    }

    /// Associates a [RetroForward] to its [NodeID]
    pub(crate) fn insert_retro_forward(
        &mut self,
        node_id: NodeID,
        retro_forward: Arc<dyn RetroForward>,
    ) {
        self.map.insert(node_id, retro_forward);
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.map.extend(other.map);
    }

    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }
}

#[derive(new, Default, Debug)]
/// Links a [NodeID] to its autodiff graph [NodeRef]
pub(crate) struct NodeTree {
    map: HashMap<NodeID, NodeRef>,
}

impl NodeTree {
    /// Gives the parents of the node in the autodiff graph
    fn parents(&self, node_id: &NodeID) -> Vec<NodeID> {
        self.map.get(node_id).unwrap().parents.clone()
    }

    // Associates a [NodeRef] to its [NodeID]
    pub(crate) fn insert_node(&mut self, node_id: NodeID, node_ref: NodeRef) {
        self.map.insert(node_id, node_ref);
    }

    pub(crate) fn extend(&mut self, other: Self) {
        self.map.extend(other.map);
    }

    pub(crate) fn print(&self) {
        println!("{:?}", self.map.keys())
    }
}

#[derive(new, Debug, Default)]
/// Struct responsible of fetching the output for a node in the autodiff graph during a backward pass
pub struct Checkpointer {
    backward_states: BackwardStates,
    retro_forwards: RetroForwards,
    node_tree: NodeTree,
}

impl Checkpointer {
    /// Gives the output of the given node, by recursively asking parents to compute themselves
    /// or give their pre-computed tensors.
    pub fn retrieve_output<T>(&mut self, node_id: NodeID) -> T
    where
        T: Clone + Send + Sync + 'static,
    {
        println!("retrieve {:?}", node_id);
        self.topological_sort(node_id.clone())
            .into_iter()
            .for_each(|node| {
                self.retro_forwards
                    .execute_retro_forward(node, &mut self.backward_states)
            });

        self.backward_states.get_state::<T>(&node_id)
    }

    /// Insert a [State::Precomputed] at [NodeID]
    pub fn checkpoint_compute(
        &mut self,
        node_ref: NodeRef,
        saved_output: Box<dyn Any + Send + Sync>,
    ) -> NodeID {
        let node_id = node_ref.id.clone();
        if let Some(state) = self.backward_states.get_mut(&node_id) {
            // TODO make it recursive
            state.increment();
        } else {
            self.node_tree.insert_node(node_id.clone(), node_ref);
            self.backward_states.insert_state(
                node_id.clone(),
                State::Computed {
                    state_content: saved_output,
                    n_required: 1,
                },
            );
        }
        node_id
    }

    /// Sorts the ancestors of NodeID in a way such that all parents come before their children
    /// Useful to avoid recursivity later when mutating the states
    fn topological_sort(&self, node_id: NodeID) -> Vec<NodeID> {
        match self.backward_states.get_state_ref(&node_id) {
            Some(state) => match state {
                State::Recompute { n_required: _ } => {
                    let mut sorted = Vec::new();
                    for parent_node in self.node_tree.parents(&node_id) {
                        if !sorted.contains(&parent_node) {
                            sorted.extend(self.topological_sort(parent_node));
                        }
                    }
                    sorted.push(node_id);
                    sorted
                }
                State::Computed {
                    state_content: _,
                    n_required: _,
                } => vec![node_id],
            },
            None => panic!("Node {:?} is not in the backward_states. ", node_id),
        }
    }

    pub fn extend(&mut self, other: Self) {
        self.backward_states.extend(other.backward_states);
        self.node_tree.extend(other.node_tree);
        self.retro_forwards.extend(other.retro_forwards);
    }

    pub fn len(&self) -> usize {
        self.backward_states.len() + self.retro_forwards.len()
    }

    pub fn checkpoint_lazy(
        &mut self,
        node_ref: NodeRef,
        retro_forward: Arc<dyn RetroForward>,
    ) -> NodeID {
        let node_id = node_ref.id.clone();
        if let Some(state) = self.backward_states.get_mut(&node_id) {
            println!("{:?}", state.n_required());
            state.increment();
            println!("{:?}", state.n_required());
        } else {
            self.node_tree.insert_node(node_id.clone(), node_ref);
            self.retro_forwards
                .insert_retro_forward(node_id.clone(), retro_forward);
            self.backward_states
                .insert_state(node_id.clone(), State::Recompute { n_required: 1 });
        }
        node_id
    }

    // // TODO TMP
    pub fn print(&self) {
        println!("\n\nCheckpointer");
        println!("\n");
        self.node_tree.print();
        println!("\n{:?}", self.backward_states);
        println!("\n{:?}", self.retro_forwards);
    }

    pub fn build(checkpointing_actions: Vec<CheckpointingAction>) -> Self {
        let mut checkpointer = Self::default();
        for action in checkpointing_actions {
            match action {
                CheckpointingAction::Compute {
                    node_ref,
                    state_content,
                } => println!("{:?}C", node_ref.id),
                CheckpointingAction::Recompute {
                    node_ref,
                    retro_forward,
                } => println!("{:?}R", node_ref.id),
            }
            //     match action {
            //         CheckpointingAction::Compute {
            //             node_ref,
            //             state_content,
            //         } => checkpointer.checkpoint_compute(node_ref, state_content),
            //         CheckpointingAction::Recompute {
            //             node_ref,
            //             retro_forward,
            //         } => checkpointer.checkpoint_lazy(node_ref, retro_forward),
            //     };
        }
        checkpointer
    }
}
