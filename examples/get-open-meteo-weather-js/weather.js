// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

export async function getWeather(city) {
  try {
    // Geocode the location using Open-Meteo
    const geoResponse = await fetch(
      `https://geocoding-api.open-meteo.com/v1/search?name=${encodeURIComponent(city)}&count=1&language=en&format=json`
    );
    if (!geoResponse.ok) {
      throw "Error: Failed to fetch geo data";
    }
    const geoData = await geoResponse.json();
    if (!geoData.results || geoData.results.length === 0) {
      throw `Error: Location '${city}' not found`;
    }

    const lat = geoData.results[0].latitude;
    const lon = geoData.results[0].longitude;

    // Get weather data from Open-Meteo
    const response = await fetch(
      `https://api.open-meteo.com/v1/forecast?latitude=${lat}&longitude=${lon}&current=temperature_2m`
    );
    if (!response.ok) {
      throw "Error: Failed to fetch weather data";
    }
    const data = await response.json();
    const weather = data.current.temperature_2m.toString();
    return weather;
  } catch (error) {
    throw error.message || "Error fetching weather data";
  }
}
