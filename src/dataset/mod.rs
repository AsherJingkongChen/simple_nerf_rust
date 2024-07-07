use burn::{ data::dataset::Dataset, prelude::*, tensor::Distribution };
use npyz::{ NpyFile, npz };
use reqwest::IntoUrl;
use std::{ io, ops::Range, path::Path };
use zip::ZipArchive;

#[derive(Config, Debug)]
pub struct SimpleNerfDatasetConfig {
    pub points_per_ray: usize,
    pub distance_range: Range<f32>,
}

#[derive(Clone, Debug)]
pub struct SimpleNerfDataset<B: Backend> {
    device: B::Device,
    inners: Vec<SimpleNerfDatasetInner>,
    pub has_noisy_distance: bool,
}

#[derive(Clone, Debug)]
struct SimpleNerfDatasetInner {
    directions: Data<f32, 4>,
    distances: Data<f32, 4>,
    image: Data<f32, 3>,
    origins: Data<f32, 4>,
}

#[derive(Clone, Debug)]
pub struct SimpleNerfDatasetItem<B: Backend> {
    pub directions: Tensor<B, 4>,
    pub distances: Tensor<B, 4>,
    pub image: Tensor<B, 3>,
    pub positions: Tensor<B, 4>,
}

#[derive(Clone, Debug)]
pub struct SimpleNerfDatasetSplit<B: Backend> {
    pub train: SimpleNerfDataset<B>,
    pub test: SimpleNerfDataset<B>,
}

impl SimpleNerfDatasetConfig {
    pub fn init_from_reader<
        B: Backend<FloatElem = f32>,
        R: io::Read + io::Seek
    >(
        &self,
        reader: R,
        device: &B::Device
    ) -> io::Result<SimpleNerfDataset<B>> {
        let parse_err = io::ErrorKind::InvalidData;
        let mut reader = ZipArchive::new(reader)?;

        let focal = *NpyFile::new(
            reader.by_name(&npz::file_name_from_array_name("focal"))?
        )?
            .into_vec::<f64>()?
            .get(0)
            .ok_or(parse_err)? as f32;

        let images = {
            let array = NpyFile::new(
                reader.by_name(&npz::file_name_from_array_name("images"))?
            )?;
            let shape = Shape::from(array.shape().to_vec());
            Tensor::<B, 4>::from_data(
                Data::new(array.into_vec()?, shape),
                device
            )
        };

        let poses = {
            let array = NpyFile::new(
                reader.by_name(&npz::file_name_from_array_name("poses"))?
            )?;
            let shape = Shape::from(array.shape().to_vec());
            Tensor::<B, 3>::from_data(
                Data::new(array.into_vec()?, shape),
                device
            )
        };

        let [image_count, height, width, channel_count] = images.dims();
        let pose_count = poses.dims()[0];
        if image_count != pose_count {
            return Err(parse_err.into());
        }
        if channel_count != 3 {
            return Err(parse_err.into());
        }

        let planes = {
            let planes_shape = [1, height, width, 1, 3];
            let plane_shape = [height, width];
            let plane_x =
                (Tensor::arange(0..width as i64, device)
                    .float()
                    .unsqueeze_dim::<2>(0)
                    .expand(plane_shape) -
                    (width as f32) / 2.0) /
                focal;
            let plane_y =
                (-Tensor::arange(0..height as i64, device)
                    .float()
                    .unsqueeze_dim::<2>(1)
                    .expand(plane_shape) +
                    (height as f32) / 2.0) /
                focal;
            let plane_z = Tensor::full(plane_shape, -1.0, device);
            Tensor::<B, 2>
                ::stack::<3>(vec![plane_x, plane_y, plane_z], 2)
                .reshape(planes_shape)
        };

        let directions = (
            planes *
            poses
                .clone()
                .slice([0..image_count, 0..3, 0..3])
                .unsqueeze_dims::<5>(&[1, 1])
        )
            .sum_dim(4)
            .swap_dims(4, 3);

        let origins = poses
            .slice([0..image_count, 0..3, 3..4])
            .unsqueeze_dims::<5>(&[1, 1])
            .swap_dims(4, 3)
            .expand(directions.shape());

        let directions = directions.repeat(3, self.points_per_ray);

        let distances = (
            Tensor::<B, 1, Int>
                ::arange(0..self.points_per_ray as i64, device)
                .float() *
                ((self.distance_range.end - self.distance_range.start) /
                    (self.points_per_ray as f32)) +
            self.distance_range.start
        )
            .unsqueeze_dims::<5>(&[0, 0, 0, -1])
            .expand([image_count, height, width, self.points_per_ray, 1]);

        let inners = directions
            .iter_dim(0)
            .zip(distances.iter_dim(0))
            .zip(images.iter_dim(0))
            .zip(origins.iter_dim(0))
            .map(|(((directions, distances), image), origins)| {
                SimpleNerfDatasetInner {
                    directions: directions.squeeze::<4>(0).into_data(),
                    distances: distances.squeeze::<4>(0).into_data(),
                    image: image.squeeze::<3>(0).into_data(),
                    origins: origins.squeeze::<4>(0).into_data(),
                }
            })
            .collect();

        Ok(SimpleNerfDataset {
            device: device.clone(),
            inners,
            has_noisy_distance: false,
        })
    }

    pub fn init_from_file_path<B: Backend<FloatElem = f32>>(
        &self,
        file_path: impl AsRef<Path>,
        device: &B::Device
    ) -> io::Result<SimpleNerfDataset<B>> {
        self.init_from_reader(std::fs::File::open(file_path)?, device)
    }

    pub fn init_from_url<B: Backend<FloatElem = f32>>(
        &self,
        url: impl IntoUrl,
        device: &B::Device
    ) -> io::Result<SimpleNerfDataset<B>> {
        self.init_from_reader(
            io::Cursor::new(
                reqwest::blocking
                    ::get(url)
                    .or(Err(io::ErrorKind::ConnectionRefused))?
                    .error_for_status()
                    .or(Err(io::ErrorKind::NotFound))?
                    .bytes()
                    .or(Err(io::ErrorKind::Interrupted))?
            ),
            device
        )
    }
}

impl<B: Backend> SimpleNerfDataset<B> {
    pub fn split_for_training(self, ratio: f32) -> SimpleNerfDatasetSplit<B> {
        let (inners_left, inners_right) = self.inners.split_at(
            (
                ratio.clamp(0.0, 1.0) * (self.inners.len() as f32)
            ).round() as usize
        );

        SimpleNerfDatasetSplit {
            train: SimpleNerfDataset {
                device: self.device.clone(),
                inners: inners_left.into(),
                has_noisy_distance: true,
            },
            test: SimpleNerfDataset {
                device: self.device,
                inners: inners_right.to_vec(),
                has_noisy_distance: false,
            },
        }
    }
}

impl<B: Backend<FloatElem = f32>> Dataset<SimpleNerfDatasetItem<B>>
for SimpleNerfDataset<B> {
    fn len(&self) -> usize {
        self.inners.len()
    }

    fn get(&self, index: usize) -> Option<SimpleNerfDatasetItem<B>> {
        let inner = self.inners.get(index)?.clone();

        let image = Tensor::from_data(inner.image, &self.device);

        let directions = Tensor::from_data(inner.directions, &self.device);

        let distance_value = {
            let values = inner.distances.value.get(0..2).unwrap_or(&[0.0, 0.0]);
            values[1] - values[0]
        };
        let mut distances = Tensor::from_data(inner.distances, &self.device);
        if self.has_noisy_distance {
            let noises = distances.random_like(
                Distribution::Uniform(0.0, distance_value as f64)
            );
            distances = distances + noises;
            println!("intv: {:?}", distance_value);
        }
        let distances = distances;

        let positions: Tensor<B, 4> = Tensor::from_data(
            inner.origins,
            &self.device
        ) +
        directions.clone() * distances.clone();

        Some(SimpleNerfDatasetItem {
            directions,
            distances,
            image,
            positions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Backend = burn::backend::Wgpu;

    const TEST_DATA_FILE_PATH: &str = "resources/lego-tiny/data.npz";
    const TEST_DATA_URL: &str =
        "https://raw.githubusercontent.com/AsherJingkongChen/simple-nerf-rust/main/resources/lego-tiny/data.npz";

    #[test]
    fn output_shape() {
        let device = Default::default();

        let dataset = (SimpleNerfDatasetConfig {
            points_per_ray: 7,
            distance_range: 2.0..6.0,
        }).init_from_file_path::<Backend>(TEST_DATA_FILE_PATH, &device);
        assert!(dataset.is_ok());

        let dataset = dataset.unwrap();
        let item = dataset.get(0);
        assert!(item.is_some());

        let item = item.unwrap();
        assert_eq!(item.directions.dims(), [100, 100, 7, 3]);
        assert_eq!(item.distances.dims(), [100, 100, 7, 1]);
        assert_eq!(item.image.dims(), [100, 100, 3]);
        assert_eq!(item.positions.dims(), [100, 100, 7, 3]);
        assert_eq!(item.positions.dims(), item.directions.dims());

        let inners = dataset.inners;
        assert_eq!(inners.len(), 106);

        let inner = inners.get(0);
        assert!(inner.is_some());

        let inner = inner.unwrap();
        assert_eq!(inner.directions.shape.dims, [100, 100, 7, 3]);
        assert_eq!(inner.distances.shape.dims, [100, 100, 7, 1]);
        assert_eq!(inner.image.shape.dims, [100, 100, 3]);
        assert_eq!(inner.origins.shape.dims, [100, 100, 1, 3]);
    }

    #[test]
    fn remote_retrieval() {
        let device = Default::default();

        let dataset = (SimpleNerfDatasetConfig {
            points_per_ray: 7,
            distance_range: 2.0..6.0,
        }).init_from_url::<Backend>(TEST_DATA_URL, &device);
        assert!(dataset.is_ok());

        let dataset = dataset.unwrap();
        assert_eq!(dataset.inners.len(), 106);
    }

    #[test]
    fn splitting() {
        let device = Default::default();

        let dataset = (SimpleNerfDatasetConfig {
            points_per_ray: 8,
            distance_range: 2.0..6.0,
        }).init_from_file_path::<Backend>(TEST_DATA_FILE_PATH, &device);
        assert!(dataset.is_ok());

        let dataset = dataset.unwrap();
        let datasets = dataset.split_for_training(0.8);
        assert_eq!(datasets.train.len(), datasets.train.inners.len());
        assert_eq!(datasets.test.len(), datasets.test.inners.len());
        assert!(datasets.train.has_noisy_distance);
        assert!(!datasets.test.has_noisy_distance);

        let datasets = datasets.train.split_for_training(1.0);
        assert_eq!(datasets.train.len(), datasets.train.inners.len());
        assert_eq!(datasets.test.len(), datasets.test.inners.len());
        assert_eq!(datasets.test.len(), 0);
        assert!(datasets.train.has_noisy_distance);
        assert!(!datasets.test.has_noisy_distance);
    }
}