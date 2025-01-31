use tracing::trace;

#[derive(Debug, Clone)]
pub struct RunningTotal {
    held_value: Vec<f32>,
    averages: Vec<Vec<f32>>,
    bins: usize,
    max_value: f32
}

impl RunningTotal {
    pub fn new(base_values: Vec<Option<f32>>, bins: usize, max_value: f32) -> Self {
        let vals = base_values.iter().map(|x| x.unwrap_or(0.0)).collect::<Vec<f32>>();
        Self {
            held_value: vals.clone(),
            averages: vec![vals],
            bins,
            max_value
        }
    }

    fn convert_to_f32(values: &Vec<Option<f32>>) -> Vec<f32> {
        return values.iter().map(|x| x.unwrap_or(0.0)).collect::<Vec<f32>>();
    }

    fn to_percentage(&self, values: &Vec<Option<f32>>) -> Vec<f32> {
        let f32ed = Self::convert_to_f32(values);
        return f32ed.iter().map(|x| (x / self.max_value) * 100.0).collect();
    }

    // everything goes to a constant, if it doesnt exist it becomes 0.0
    fn elementwise_subtraction(vec_a: &Vec<f32>, vec_b: &Vec<f32>) -> Vec<f32> {
        if vec_a.len() != vec_b.len() {
            panic!("Vectors must have the same length!");
        }
        let mut out_vec = vec![];
        for i in 0..vec_a.len() {
            let val_a = vec_a.get(i).unwrap();
            let val_b = vec_b.get(i).unwrap();
            out_vec.push(val_b - val_a);
        }

        out_vec
    }

    pub fn add_values(&mut self, new_values: &Vec<Option<f32>>) {
        //let zeroed_values = Self::convert_to_f32(new_values);
        //let added = Self::elementwise_subtraction(&self.held_value, &zeroed_values); this allows us to do the 1024 sliding window instead of working on new elements, somewhat makes held_value irrelevant
        let norm = self.to_percentage(new_values);
        self.averages.push(norm.clone());
        self.held_value =  norm.clone();
        if self.averages.len() > self.bins {
            self.averages.remove(0);
        }
        trace!("Adding values to running total: {:?}", norm);
        trace!("Current recorded values: {:?}", self.averages);
    }

    pub fn add_values_checked(&mut self, new_values: &Vec<Option<f32>>) -> bool {
        for val in new_values {
            if val.is_none() {
                return false;
            }
        }
        self.add_values(new_values);
        true
    }

    pub fn get_average(&self) -> Option<Vec<f32>> {
        if self.averages.len() < self.bins {
            return None;
        }
        let mut output = vec![];
        for i in 0..self.averages.get(0).unwrap().len() {
            let mut sum = 0.0;
            for average in self.averages.iter() {
                if let Some(val) = average.get(i) {
                    sum += val;
                }
            }
            output.push(sum / self.averages.len() as f32);
        }
        Some(output)
    }

    /*pub fn get_singluar_average(&self) -> Option<f32> {
        let averages = self.get_average();
        if averages.is_none() {
            return None;
        }
        let mut sum = 0.0;
        for average in averages.unwrap() {
            sum += average;
        }
        Some(sum / self.held_value.len() as f32)
    }*/
}