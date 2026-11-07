"""A single positive temperature fitted without touching evaluation examples."""

from dataclasses import dataclass
import math

from .contract import distribution


@dataclass(frozen=True)
class Observation:
    profile: str
    group: str
    partition: str
    stage: str
    source: str
    scores: dict[str, float]
    selected: str
    label: str
    weight: float

    @classmethod
    def parse(cls, value):
        if not isinstance(value, dict) or set(value) != set(cls.__dataclass_fields__):
            raise ValueError("invalid calibration observation")
        result = cls(**value)
        if not result.profile or not isinstance(result.profile, str):
            raise ValueError("calibration profile is required")
        if not result.group or not isinstance(result.group, str):
            raise ValueError("calibration group is required")
        if result.partition not in ("calibration", "evaluation"):
            raise ValueError("unknown calibration partition")
        if result.source not in ("explicit", "inferred", "confirmed", "uncorrected", "curated"):
            raise ValueError("unknown calibration label source")
        if not result.stage or not isinstance(result.stage, str):
            raise ValueError("calibration stage is required")
        if not isinstance(result.scores, dict) or len(result.scores) < 2 or result.label not in result.scores or result.selected not in result.scores:
            raise ValueError("the label must have a candidate score")
        if any(not isinstance(key, str) or not key for key in result.scores):
            raise ValueError("candidate labels must be nonempty strings")
        numbers = [*result.scores.values(), result.weight]
        if any(isinstance(number, bool) or not isinstance(number, (int, float)) or not math.isfinite(number) for number in numbers):
            raise ValueError("scores and weights must be finite numbers")
        if result.weight <= 0:
            raise ValueError("calibration weight must be positive")
        if result.scores[result.selected] != max(result.scores.values()):
            raise ValueError("recorded selection does not match the model scores")
        return result


def validate_partitions(observations):
    partitions = {}
    for observation in observations:
        previous = partitions.setdefault(observation.group, observation.partition)
        if previous != observation.partition:
            raise ValueError("a request group cannot appear in both partitions")


def nll(observations, temperature):
    if not math.isfinite(temperature) or temperature <= 0 or not observations:
        raise ValueError("NLL needs observations and a positive temperature")
    total = 0.0
    weight = 0.0
    for observation in observations:
        maximum = max(observation.scores.values())
        shifted = [(score - maximum) / temperature for score in observation.scores.values()]
        loss = math.log(sum(math.exp(score) for score in shifted))
        loss -= (observation.scores[observation.label] - maximum) / temperature
        total += observation.weight * loss
        weight += observation.weight
    return total / weight


def fit(observations, minimum=0.05, maximum=20.0):
    """Bisect the monotone NLL derivative with respect to inverse temperature."""
    validate_partitions(observations)
    if not observations or any(row.partition != "calibration" for row in observations):
        raise ValueError("fit accepts only the calibration partition")
    if len({row.profile for row in observations}) != 1:
        raise ValueError("different model or prompt profiles cannot share a fit")
    if not 0 < minimum < maximum or not math.isfinite(maximum):
        raise ValueError("invalid temperature bounds")

    def derivative(inverse):
        total = 0.0
        for row in observations:
            scores = list(row.scores.values())
            probabilities = distribution(scores, 1.0 / inverse)
            expected = sum(score * probability for score, probability in zip(scores, probabilities))
            total += row.weight * (expected - row.scores[row.label])
        return total

    low, high = 1.0 / maximum, 1.0 / minimum
    if derivative(low) >= 0:
        temperature = maximum
    elif derivative(high) <= 0:
        temperature = minimum
    else:
        for _ in range(80):
            middle = (low + high) / 2
            if derivative(middle) < 0:
                low = middle
            else:
                high = middle
        temperature = 2 / (low + high)
    return {
        "temperature": temperature,
        "bounds": [minimum, maximum],
        "atBound": temperature in (minimum, maximum),
        "observations": len(observations),
        "groups": len({row.group for row in observations}),
        "nllBefore": nll(observations, 1.0),
        "nllAfter": nll(observations, temperature),
    }


def metrics(observations, temperature):
    if not observations:
        return {"observations": 0}
    correct = weight = 0.0
    bins = [[] for _ in range(15)]
    stages = {}
    for row in observations:
        labels = list(row.scores)
        probabilities = distribution(list(row.scores.values()), temperature)
        selected = labels.index(row.selected)
        confidence = probabilities[selected]
        matches = labels[selected] == row.label
        correct += matches * row.weight
        weight += row.weight
        bins[min(14, int(confidence * 15))].append((confidence, matches, row.weight))
        count, errors = stages.get(row.stage, (0, 0))
        stages[row.stage] = (count + 1, errors + int(not matches))
    ece = 0.0
    for bucket in bins:
        bucket_weight = sum(item[2] for item in bucket)
        if bucket_weight:
            confidence = sum(item[0] * item[2] for item in bucket) / bucket_weight
            accuracy = sum(item[1] * item[2] for item in bucket) / bucket_weight
            ece += bucket_weight / weight * abs(accuracy - confidence)
    return {
        "observations": len(observations),
        "groups": len({row.group for row in observations}),
        "nll": nll(observations, temperature),
        "accuracy": correct / weight,
        "ece": ece,
        "stageErrors": {stage: {"samples": count, "errors": errors} for stage, (count, errors) in stages.items()},
    }
