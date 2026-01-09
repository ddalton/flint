# Real-World Use Cases for Direct SPDK Access

## Overview

Applications that benefit most from 6+ GB/s throughput (2x faster than CSI):
- High sequential I/O workloads
- Large dataset processing
- I/O-bound operations that bottleneck on storage

---

## 1. Data Engineering

### A. Apache Spark Shuffle Operations

**Problem**: Spark shuffle writes/reads are often the bottleneck in large joins and aggregations.

**Why Direct SPDK Helps**:
- Shuffle can generate 100s of GB to TBs of intermediate data
- Sequential writes during shuffle write phase
- Sequential reads during shuffle read phase
- **2x throughput = 2x faster shuffles**

**Example Pod Configuration**:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: spark-executor-high-perf
spec:
  containers:
  - name: spark
    image: apache/spark:3.5.0
    env:
    - name: SPARK_LOCAL_DIRS
      value: /spdk-scratch  # Use SPDK-backed scratch space
    resources:
      limits:
        flint.io/nvme: 1  # Direct SPDK access
        hugepages-2Mi: 2Gi
        cpu: "8"
        memory: "32Gi"
    volumeMounts:
    - name: spdk-scratch
      mountPath: /spdk-scratch
  volumes:
  - name: spdk-scratch
    emptyDir:
      medium: ""  # Backed by SPDK device plugin
```

**Performance Impact**:
- CSI (3 GB/s): 100GB shuffle = ~33 seconds
- SPDK (6 GB/s): 100GB shuffle = ~17 seconds
- **2x faster job completion** for shuffle-heavy workloads

**Example Workload**:
```python
# PySpark - Large join that generates heavy shuffle
df1 = spark.read.parquet("s3://data/table1")  # 500GB
df2 = spark.read.parquet("s3://data/table2")  # 300GB

# This join will shuffle ~200GB
result = df1.join(df2, "key").groupBy("category").agg(...)

# With SPDK: Shuffle I/O is 2x faster
result.write.parquet("s3://output/")
```

---

### B. Parquet File Processing (Data Lake)

**Problem**: Reading/writing Parquet files for analytics is I/O intensive.

**Why Direct SPDK Helps**:
- Parquet uses columnar compression
- Large sequential reads for column scanning
- High throughput for predicate pushdown

**Example: DuckDB Analytics Query**:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: duckdb-analytics
spec:
  containers:
  - name: duckdb
    image: duckdb/duckdb:latest
    command:
      - /bin/sh
      - -c
      - |
        # Query 1TB Parquet dataset
        duckdb -c "
          COPY (
            SELECT region, product, SUM(sales)
            FROM read_parquet('/data/sales/*.parquet')  -- 1TB dataset
            WHERE date >= '2024-01-01'
            GROUP BY region, product
          ) TO '/output/summary.parquet'
        "
    resources:
      limits:
        flint.io/nvme: 1
        hugepages-2Mi: 4Gi
    volumeMounts:
    - name: data-cache
      mountPath: /data
```

**Performance**:
- Scanning 1TB Parquet with CSI: ~5 minutes (3 GB/s read)
- Scanning 1TB Parquet with SPDK: ~2.5 minutes (6 GB/s read)

---

### C. Apache Kafka - High-Throughput Log Segments

**Problem**: Kafka brokers write logs sequentially and read them for consumers.

**Why Direct SPDK Helps**:
- Sequential writes to log segments
- Sequential reads for consumer catch-up
- Log compaction is I/O intensive

**Example Kafka Broker**:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: kafka-broker-perf
spec:
  containers:
  - name: kafka
    image: confluentinc/cp-kafka:7.5.0
    env:
    - name: KAFKA_LOG_DIRS
      value: /kafka-logs
    - name: KAFKA_NUM_IO_THREADS
      value: "16"
    resources:
      limits:
        flint.io/nvme: 1
        hugepages-2Mi: 2Gi
    volumeMounts:
    - name: kafka-logs
      mountPath: /kafka-logs
```

**Throughput Impact**:
- CSI: ~300k msg/s (3 GB/s)
- SPDK: ~600k msg/s (6 GB/s)
- **2x message throughput** for high-volume topics

---

## 2. Data Warehouse / Analytics

### A. ClickHouse - Analytical Database

**Problem**: ClickHouse is designed for fast analytics but is extremely I/O intensive.

**Why Direct SPDK Helps**:
- MergeTree engine does heavy sequential reads
- Background merges are I/O intensive
- Large scans for analytical queries

**Example ClickHouse Deployment**:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: clickhouse-analytics
spec:
  containers:
  - name: clickhouse
    image: clickhouse/clickhouse-server:latest
    env:
    - name: CLICKHOUSE_DATA_DIR
      value: /var/lib/clickhouse
    resources:
      limits:
        flint.io/nvme: 1
        hugepages-2Mi: 8Gi
        cpu: "16"
        memory: "64Gi"
    volumeMounts:
    - name: clickhouse-data
      mountPath: /var/lib/clickhouse
```

**Query Performance**:
```sql
-- Scan 500GB table for aggregation
SELECT
  toStartOfHour(timestamp) as hour,
  COUNT(*) as events,
  AVG(response_time) as avg_latency
FROM events
WHERE timestamp >= now() - INTERVAL 7 DAY
GROUP BY hour
ORDER BY hour;

-- CSI (3 GB/s): 2.5 minutes
-- SPDK (6 GB/s): 1.3 minutes
```

**Real Impact**:
- Interactive query latency cut in half
- Background merges complete faster
- More concurrent analytical queries

---

### B. TimescaleDB - Time-Series Data

**Problem**: Time-series databases ingest high volumes and run range scans.

**Why Direct SPDK Helps**:
- Continuous high-rate inserts
- Large sequential scans for time ranges
- Compression/decompression is I/O bound

**Example TimescaleDB**:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: timescaledb-metrics
spec:
  containers:
  - name: timescaledb
    image: timescale/timescaledb:latest-pg16
    env:
    - name: PGDATA
      value: /var/lib/postgresql/data
    resources:
      limits:
        flint.io/nvme: 1
        hugepages-2Mi: 4Gi
```

**Workload**:
```sql
-- Insert 1M metrics/second (IoT sensors)
INSERT INTO metrics (timestamp, sensor_id, value)
VALUES (now(), 'sensor_001', 23.5), ...;

-- Query last 24 hours of metrics
SELECT time_bucket('1 minute', timestamp) AS minute,
       AVG(value) as avg_value
FROM metrics
WHERE timestamp > now() - INTERVAL '24 hours'
  AND sensor_id IN (SELECT id FROM active_sensors)
GROUP BY minute;

-- CSI: 8 seconds to scan 200GB
-- SPDK: 4 seconds to scan 200GB
```

---

## 3. AI/ML Workloads

### A. Model Training - Dataset Loading

**Problem**: GPU training is often I/O bound waiting for training data.

**Why Direct SPDK Helps**:
- Training datasets can be 100s of GB to TBs
- Each epoch reads entire dataset
- Shuffling reads data in random order (but large batches)

**Example PyTorch Training**:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: pytorch-training
spec:
  containers:
  - name: trainer
    image: pytorch/pytorch:2.1.0-cuda12.1-cudnn8-runtime
    command:
      - python
      - train.py
    resources:
      limits:
        nvidia.com/gpu: 8  # 8x A100 GPUs
        flint.io/nvme: 1   # Direct SPDK for data
        hugepages-2Mi: 8Gi
    volumeMounts:
    - name: training-data
      mountPath: /data
```

**Training Script**:
```python
import torch
from torch.utils.data import DataLoader

# Load dataset from SPDK-backed storage
dataset = ImageDataset('/data/imagenet')  # 150GB dataset
loader = DataLoader(
    dataset,
    batch_size=2048,  # Large batches
    num_workers=32,   # Parallel I/O
    prefetch_factor=4
)

for epoch in range(100):
    for batch in loader:
        # Each epoch reads 150GB
        # CSI: 50 seconds/epoch for I/O
        # SPDK: 25 seconds/epoch for I/O

        output = model(batch)
        loss = criterion(output, labels)
        loss.backward()
```

**Impact**:
- **2x faster data loading** = more GPU utilization
- Fewer GPUs idle waiting for data
- Faster experimentation cycles

---

### B. Vector Database for AI Embeddings

**Problem**: Embedding lookups for RAG (Retrieval Augmented Generation) are I/O intensive.

**Why Direct SPDK Helps**:
- Billions of embeddings stored on disk
- k-NN search reads many vectors
- High query throughput needed

**Example: Milvus Vector DB**:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: milvus-vectordb
spec:
  containers:
  - name: milvus
    image: milvusdb/milvus:v2.3.0
    env:
    - name: ETCD_USE_EMBED
      value: "true"
    resources:
      limits:
        flint.io/nvme: 1
        hugepages-2Mi: 16Gi
    volumeMounts:
    - name: milvus-data
      mountPath: /var/lib/milvus
```

**Workload**:
```python
from pymilvus import Collection

# Store 100M embeddings (768 dimensions each = ~300GB)
collection = Collection("embeddings")

# Search for top-100 similar vectors
query_embedding = model.encode("search query")
results = collection.search(
    data=[query_embedding],
    anns_field="embedding",
    param={"metric_type": "L2", "params": {"nprobe": 128}},
    limit=100
)

# CSI: 200ms latency (reads ~2GB for search)
# SPDK: 100ms latency (2x faster reads)
```

**Real-World Impact**:
- **2x faster semantic search** for RAG systems
- More queries/second for AI applications
- Better user experience for chatbots

---

### C. Model Checkpointing - Large Language Models

**Problem**: Saving/loading model checkpoints during training is I/O intensive.

**Why Direct SPDK Helps**:
- Large models (70B+ parameters) = 100s of GB checkpoints
- Frequent checkpointing for fault tolerance
- Loading checkpoints to resume training

**Example: LLaMA Training**:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: llama-training
spec:
  containers:
  - name: trainer
    image: huggingface/transformers-pytorch-gpu:latest
    resources:
      limits:
        nvidia.com/gpu: 8
        flint.io/nvme: 1
        hugepages-2Mi: 16Gi
```

**Training Code**:
```python
# LLaMA-70B checkpoint = ~140GB
model = LlamaForCausalLM.from_pretrained("llama-70b")

# Save checkpoint every 1000 steps
for step in range(100000):
    # Training...

    if step % 1000 == 0:
        # Save 140GB checkpoint
        # CSI: ~47 seconds (3 GB/s write)
        # SPDK: ~23 seconds (6 GB/s write)
        model.save_pretrained(f"/checkpoints/step-{step}")

# Resume from checkpoint
# CSI: 47 seconds to load
# SPDK: 23 seconds to load
```

**Impact**:
- **2x faster checkpointing** = less training interruption
- Faster recovery from failures
- More frequent checkpoints = better fault tolerance

---

## 4. Real-Time Analytics

### A. Apache Druid - Real-Time OLAP

**Problem**: Druid ingests real-time data and serves low-latency analytical queries.

**Why Direct SPDK Helps**:
- High-rate data ingestion
- Segment creation and merging
- Fast query scans

**Example Druid Historical Node**:
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: druid-historical
spec:
  containers:
  - name: druid
    image: apache/druid:26.0.0
    env:
    - name: druid_segmentCache_locations
      value: '[{"path":"/druid/segment-cache","maxSize":1000000000000}]'
    resources:
      limits:
        flint.io/nvme: 1
        hugepages-2Mi: 8Gi
```

**Query Performance**:
```sql
-- Real-time dashboard query
SELECT
  FLOOR(__time TO HOUR) AS hour,
  country,
  SUM(requests) AS total_requests,
  AVG(latency_ms) AS avg_latency
FROM events
WHERE __time >= CURRENT_TIMESTAMP - INTERVAL '24' HOUR
GROUP BY 1, 2
ORDER BY total_requests DESC
LIMIT 100;

-- CSI: 1.2 seconds (scans 500GB)
-- SPDK: 0.6 seconds (2x faster)
```

---

## Performance Comparison Table

| Use Case | Dataset Size | CSI (3 GB/s) | SPDK (6 GB/s) | Speedup |
|----------|-------------|--------------|---------------|---------|
| **Spark Shuffle** | 100 GB | 33s | 17s | 2x |
| **Parquet Scan** | 1 TB | 5 min | 2.5 min | 2x |
| **Kafka Throughput** | Continuous | 300k msg/s | 600k msg/s | 2x |
| **ClickHouse Query** | 500 GB | 2.5 min | 1.3 min | 1.9x |
| **TimescaleDB Scan** | 200 GB | 8s | 4s | 2x |
| **ML Training Epoch** | 150 GB | 50s | 25s | 2x |
| **Vector Search** | 300 GB | 200ms | 100ms | 2x |
| **LLM Checkpoint** | 140 GB | 47s | 23s | 2x |
| **Druid Query** | 500 GB | 1.2s | 0.6s | 2x |

---

## When to Use Direct SPDK vs CSI

### Use Direct SPDK (6+ GB/s) When:
✅ **Single large workload** per device (Kafka broker, database instance)
✅ **I/O is the bottleneck** (profiling shows high iowait)
✅ **Sequential I/O dominant** (>80% sequential)
✅ **Large I/O sizes** (128KB+ per operation)
✅ **Latency sensitive** (analytics dashboards, real-time systems)

### Use CSI (3-4 GB/s) When:
✅ **Multiple small workloads** sharing storage
✅ **PVC lifecycle management** needed (snapshots, clones)
✅ **Standard Kubernetes apps** (WordPress, Redis, etc.)
✅ **Small random I/O** (<4KB per operation)
✅ **Multi-tenant environment** (many teams, many apps)

---

## Quick Start Examples

### 1. Spark with Direct SPDK
```bash
# Reserve device
kubectl edit configmap flint-reserved-devices -n flint-system
# Add: 0000:02:00.0

# Deploy Spark with device plugin
helm install spark bitnami/spark \
  --set worker.resources.limits."flint\.io/nvme"=1 \
  --set worker.resources.limits."hugepages-2Mi"=4Gi
```

### 2. ClickHouse with Direct SPDK
```bash
# Reserve device via dashboard UI
# Click "Reserve for Plugin/Direct Use"

# Deploy ClickHouse
kubectl apply -f clickhouse-spdk.yaml

# Verify performance
clickhouse-client --query "SELECT formatReadableSize(sum(bytes)) FROM system.parts"
```

### 3. PyTorch Training with Direct SPDK
```bash
# Reserve device
# Install device plugin
helm install spdk-device-plugin ./spdk-device-plugin-chart

# Launch training
kubectl apply -f pytorch-training-job.yaml

# Monitor I/O performance
kubectl exec -it pytorch-training -- iostat -x 1
```

---

## Conclusion

Direct SPDK access (6+ GB/s) provides **2x performance** over CSI-managed storage (3 GB/s) for:
- 📊 **Data Engineering**: Spark, Kafka, Parquet processing
- 🏢 **Data Warehouses**: ClickHouse, TimescaleDB, Druid
- 🤖 **AI/ML**: Training data loading, embeddings, checkpointing

The tradeoff is simplicity (no PVCs) vs performance (2x faster), making it ideal for single high-performance workloads per device.
